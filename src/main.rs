#[allow(deprecated)]
mod args;
mod bdk_utils;
pub mod bitcoind_client;
mod cli;
mod convert;
mod disk;
mod error;
mod hex_utils;
mod proxy;
mod rgb_utils;
mod swap;

use crate::bdk_utils::{broadcast_tx, get_bdk_wallet, get_bdk_wallet_seckey, sync_wallet};
use crate::bitcoind_client::BitcoindClient;
use crate::disk::FilesystemLogger;
use crate::proxy::Proxy;
use crate::rgb_utils::get_rgb_node_client;
use crate::rgb_utils::{get_asset_owned_values, RgbUtilities};
use crate::swap::SwapType;
use amplify::bmap;
use bdk::bitcoin::OutPoint;
use bdk::database::SqliteDatabase;
use bdk::Wallet;
use bdk::{FeeRate, SignOptions};
use bitcoin::blockdata::transaction::Transaction;
use bitcoin::hashes::hex::FromHex;
use bitcoin::hashes::Hash;
use bitcoin::network::constants::Network;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::util::bip32::{ChildNumber, ExtendedPrivKey};
use bitcoin::{BlockHash, PackedLockTime, Script, Sequence, TxIn, Witness};
use bitcoin_bech32::WitnessProgram;
use bp::seals::txout::CloseMethod;
use lightning::chain;
use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator};
use lightning::chain::keysinterface::{
	DelayedPaymentOutputDescriptor, EntropySource, InMemorySigner, KeysManager,
	SpendableOutputDescriptor,
};
use lightning::chain::{chainmonitor, ChannelMonitorUpdateStatus};
use lightning::chain::{Filter, Watch};
use lightning::ln::channelmanager;
use lightning::ln::channelmanager::{
	ChainParameters, ChannelManagerReadArgs, SimpleArcChannelManager,
};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler, SimpleArcPeerManager};
use lightning::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use lightning::onion_message::SimpleArcOnionMessenger;
use lightning::rgb_utils::{get_rgb_channel_info, is_channel_rgb, RgbUtxo, RgbUtxos, RgbInfo};
use lightning::routing::gossip;
use lightning::routing::gossip::{NodeId, P2PGossipSync};
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::ProbabilisticScorerUsingTime;
use lightning::util::config::UserConfig;
use lightning::util::events::{Event, PaymentPurpose};
use lightning::util::ser::ReadableArgs;
use lightning_background_processor::{BackgroundProcessor, GossipSync};
use lightning_block_sync::init;
use lightning_block_sync::poll;
use lightning_block_sync::SpvClient;
use lightning_block_sync::UnboundedCache;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::FilesystemPersister;
use lnpbp::chain::{Chain, GENESIS_HASH_REGTEST};
use psbt::{Psbt, PsbtVersion};
use rand::{thread_rng, Rng};
use reqwest::Client as RestClient;
use rgb::{Node, ContractId};
use rgb::{
	seal, Assignment, Consignment, EndpointValueMap, PedersenStrategy, SealEndpoint, StateTransfer,
	TypedAssignments,
};
use rgb_rpc::{Client, ContractValidity, Reveal};
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap};
use std::convert::TryInto;
use std::fmt;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use strict_encoding::{StrictDecode, StrictEncode};

const FEE_RATE: f32 = 1.5;
const ELECTRUM_URL_REGTEST: &str = "127.0.0.1:50001";
const ELECTRUM_URL_TESTNET: &str = "ssl://electrum.iriswallet.com:50013";
const PROXY_URL_REGTEST: &str = "http://127.0.0.1:3000/json-rpc";
const PROXY_URL_TESTNET: &str = "https://proxy.iriswallet.com/json-rpc";
const PROXY_TIMEOUT: u8 = 90;
const UTXO_SIZE_SAT: u64 = 1000;

pub(crate) enum HTLCStatus {
	Pending,
	Succeeded,
	Failed,
}

pub(crate) struct MillisatAmount(Option<u64>);

impl fmt::Display for MillisatAmount {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self.0 {
			Some(amt) => write!(f, "{}", amt),
			None => write!(f, "unknown"),
		}
	}
}

pub(crate) struct PaymentInfo {
	preimage: Option<PaymentPreimage>,
	secret: Option<PaymentSecret>,
	status: HTLCStatus,
	amt_msat: MillisatAmount,
}

pub(crate) type PaymentInfoStorage = Arc<Mutex<HashMap<PaymentHash, PaymentInfo>>>;

type ChainMonitor = chainmonitor::ChainMonitor<
	InMemorySigner,
	Arc<dyn Filter + Send + Sync>,
	Arc<BitcoindClient>,
	Arc<BitcoindClient>,
	Arc<FilesystemLogger>,
	Arc<FilesystemPersister>,
>;

pub(crate) type PeerManager = SimpleArcPeerManager<
	SocketDescriptor,
	ChainMonitor,
	BitcoindClient,
	BitcoindClient,
	BitcoindClient,
	FilesystemLogger,
>;

pub(crate) type Scorer = ProbabilisticScorerUsingTime<Arc<NetworkGraph>, Arc<FilesystemLogger>, std::time::Instant>;

pub(crate) type Router = DefaultRouter<Arc<NetworkGraph>, Arc<FilesystemLogger>, Arc<Mutex<Scorer>>>;

pub(crate) type ChannelManager =
	SimpleArcChannelManager<ChainMonitor, BitcoindClient, BitcoindClient, FilesystemLogger>;

pub(crate) type NetworkGraph = gossip::NetworkGraph<Arc<FilesystemLogger>>;

type OnionMessenger = SimpleArcOnionMessenger<FilesystemLogger>;

async fn handle_ldk_events(
	channel_manager: &Arc<ChannelManager>, bitcoind_client: &BitcoindClient,
	network_graph: &NetworkGraph, keys_manager: &KeysManager,
	inbound_payments: &PaymentInfoStorage, outbound_payments: &PaymentInfoStorage,
	network: Network, event: &Event, ldk_data_dir: String, rgb_node_client: Arc<Mutex<Client>>,
	proxy_client: Arc<RestClient>, proxy_url: &str, wallet_arc: Arc<Mutex<Wallet<SqliteDatabase>>>,
	electrum_url: String, whitelisted_trades: &Arc<Mutex<HashMap<PaymentHash, (ContractId, SwapType)>>>
) {
	match event {
		Event::FundingGenerationReady {
			temporary_channel_id,
			counterparty_node_id,
			channel_value_satoshis,
			output_script,
			..
		} => {
			struct RgbState {
				rgb_change_amount: u64,
				channel_rgb_amount: u64,
				rgb_inputs: Vec<OutPoint>,
				rgb_info: RgbInfo,
			}

			let addr = WitnessProgram::from_scriptpubkey(
				&output_script[..],
				match network {
					Network::Bitcoin => bitcoin_bech32::constants::Network::Bitcoin,
					Network::Testnet => bitcoin_bech32::constants::Network::Testnet,
					Network::Regtest => bitcoin_bech32::constants::Network::Regtest,
					Network::Signet => bitcoin_bech32::constants::Network::Signet,
				},
			)
			.expect("Lightning funding tx should always be to a SegWit output")
			.to_scriptpubkey();
			let script = Script::from_byte_iter(addr.clone().into_iter().map(|b| Ok(b)))
				.expect("valid script");

			let ldk_data_dir = PathBuf::from(&ldk_data_dir);
			let is_colored = is_channel_rgb(temporary_channel_id, &ldk_data_dir);
			let rgb_state = if is_colored {
				let (rgb_info, _) =
					get_rgb_channel_info(temporary_channel_id, &ldk_data_dir);
				let channel_rgb_amount: u64 = rgb_info.local_rgb_amount;
				let asset_owned_values = get_asset_owned_values(
					rgb_info.contract_id,
					rgb_node_client.clone(),
					wallet_arc.clone(),
					electrum_url.clone(),
				)
				.expect("known contract");
				let mut rgb_inputs: Vec<OutPoint> = vec![];
				let mut input_amount: u64 = 0;
				for owned_value in asset_owned_values {
					if input_amount >= channel_rgb_amount {
						break;
					}
					let outpoint =
						OutPoint { txid: owned_value.seal.txid, vout: owned_value.seal.vout };
					rgb_inputs.push(outpoint);
					input_amount += owned_value.state.value
				}
				let rgb_change_amount = input_amount - channel_rgb_amount;

				Some (RgbState {
					channel_rgb_amount,
					rgb_change_amount,
					rgb_inputs,
					rgb_info,
				})
			} else {
				None
			};

			let rgb_utxos_path = format!("{}/rgb_utxos", ldk_data_dir.display());
			let serialized_utxos =
				fs::read_to_string(&rgb_utxos_path).expect("able to read rgb utxos file");
			let mut rgb_utxos: RgbUtxos =
				serde_json::from_str(&serialized_utxos).expect("valid rgb utxos");
			let unspendable_utxos: Vec<OutPoint> = rgb_utxos
				.utxos
				.iter()
				.filter(|u| {
					rgb_state.as_ref().map(|state| !state.rgb_inputs.contains(&u.outpoint)).unwrap_or(true)
				})
				.map(|u| u.outpoint)
				.collect();
			let wallet = wallet_arc.lock().unwrap();
			let mut builder = wallet.build_tx();
			if let Some(rgb_state) = &rgb_state {
				builder.add_utxos(&rgb_state.rgb_inputs).expect("valid utxos");
			}
			builder
				.unspendable(unspendable_utxos)
				.fee_rate(FeeRate::from_sat_per_vb(FEE_RATE))
				.ordering(bdk::wallet::tx_builder::TxOrdering::Untouched)
				.add_recipient(script, *channel_value_satoshis);
			let psbt = builder.finish().expect("valid psbt finish").0;

			let (mut psbt, state_and_consignment) = match rgb_state {
				Some(rgb_state) => {
					let mut beneficiaries: EndpointValueMap = bmap![
						SealEndpoint::WitnessVout {
							method: CloseMethod::OpretFirst,
							vout: 0,
							blinding: 777,
						} => rgb_state.channel_rgb_amount
					];
					if rgb_state.rgb_change_amount > 0 {
						beneficiaries.insert(
							SealEndpoint::WitnessVout {
								method: CloseMethod::OpretFirst,
								vout: 1,
								blinding: 777,
							},
							rgb_state.rgb_change_amount,
						);
					}
					let input_outpoints_bt: BTreeSet<OutPoint> = rgb_state.rgb_inputs.clone().into_iter().collect();
					let mut rgb_client = rgb_node_client.lock().unwrap();
					let (psbt, consignment) = rgb_client.send_rgb(
						rgb_state.rgb_info.contract_id,
						psbt,
						input_outpoints_bt,
						beneficiaries,
						vec![],
					);
					(psbt, Some((rgb_state, consignment)))
				},
				None => { (psbt, None) }
			};

			// Sign the final funding transaction
			wallet.sign(&mut psbt, SignOptions::default()).expect("able to sign");
			let funding_tx = psbt.extract_tx();
			let funding_txid = funding_tx.txid();

			if let Some((rgb_state, consignment)) = state_and_consignment {
				let consignment_path = format!("{}/consignment_{funding_txid}", ldk_data_dir.display());
				consignment.strict_file_save(consignment_path.clone()).expect("consignment save ok");

				if rgb_state.rgb_change_amount > 0 {
					let rgb_change_utxo =
						RgbUtxo { outpoint: OutPoint { txid: funding_txid, vout: 2 }, colored: true };
					rgb_utxos.utxos.push(rgb_change_utxo);
					let serialized_utxos = serde_json::to_string(&rgb_utxos).expect("valid rgb utxos");
					fs::write(rgb_utxos_path, serialized_utxos).expect("able to write rgb utxos file");

					let funding_consignment_path =
						format!("{}/consignment_{}", ldk_data_dir.display(), hex::encode(&temporary_channel_id));
					consignment
						.strict_file_save(funding_consignment_path.clone())
						.expect("consignment save ok");
				}

				let proxy_ref = (*proxy_client).clone();
				let res = proxy_ref.post_consignment(
					proxy_url,
					funding_txid.to_string(),
					consignment_path.into(),
				);
				if res.is_err() || res.unwrap().result.is_none() {
					println!("ERROR: unable to post consignment");
					return;
				}

				let reveal = Reveal {
					blinding_factor: 777,
					outpoint: OutPoint { txid: funding_txid, vout: 0 },
					close_method: CloseMethod::OpretFirst,
					witness_vout: true,
				};
				let mut rgb_client = rgb_node_client.lock().unwrap();
				let _status = rgb_client
					.consume_transfer(consignment, true, Some(reveal), |_| ())
					.expect("valid consume tranfer");
			}

			// Give the funding transaction back to LDK for opening the channel.
			if channel_manager
				.funding_transaction_generated(
					&temporary_channel_id,
					counterparty_node_id,
					funding_tx,
				)
				.is_err()
			{
				println!(
					"\nERROR: Channel went away before we could fund it. The peer disconnected or refused the channel.");
			} else {
				println!("FUNDING COMPLETED");
			}

			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::PaymentClaimable {
			payment_hash,
			purpose,
			amount_msat,
			receiver_node_id: _,
			via_channel_id: _,
			via_user_channel_id: _,
		} => {
			println!(
				"\nEVENT: received payment from payment hash {} of {} millisatoshis",
				hex_utils::hex_str(&payment_hash.0),
				amount_msat,
			);
			print!("> ");
			io::stdout().flush().unwrap();
			let payment_preimage = match purpose {
				PaymentPurpose::InvoicePayment { payment_preimage, .. } => *payment_preimage,
				PaymentPurpose::SpontaneousPayment(preimage) => Some(*preimage),
			};
			channel_manager.claim_funds(payment_preimage.unwrap());
		}
		Event::PaymentClaimed { payment_hash, purpose, amount_msat, receiver_node_id: _ } => {
			println!(
				"\nEVENT: claimed payment from payment hash {} of {} millisatoshis",
				hex_utils::hex_str(&payment_hash.0),
				amount_msat,
			);
			print!("> ");
			io::stdout().flush().unwrap();
			let (payment_preimage, payment_secret) = match purpose {
				PaymentPurpose::InvoicePayment { payment_preimage, payment_secret, .. } => {
					(*payment_preimage, Some(*payment_secret))
				}
				PaymentPurpose::SpontaneousPayment(preimage) => (Some(*preimage), None),
			};
			let mut payments = inbound_payments.lock().unwrap();
			match payments.entry(*payment_hash) {
				Entry::Occupied(mut e) => {
					let payment = e.get_mut();
					payment.status = HTLCStatus::Succeeded;
					payment.preimage = payment_preimage;
					payment.secret = payment_secret;
				}
				Entry::Vacant(e) => {
					e.insert(PaymentInfo {
						preimage: payment_preimage,
						secret: payment_secret,
						status: HTLCStatus::Succeeded,
						amt_msat: MillisatAmount(Some(*amount_msat)),
					});
				}
			}
			println!("Event::PaymentClaimed end");
		}
		Event::PaymentSent { payment_preimage, payment_hash, fee_paid_msat, .. } => {
			let mut payments = outbound_payments.lock().unwrap();
			for (hash, payment) in payments.iter_mut() {
				if *hash == *payment_hash {
					payment.preimage = Some(*payment_preimage);
					payment.status = HTLCStatus::Succeeded;
					println!(
						"\nEVENT: successfully sent payment of {} millisatoshis{} from \
								 payment hash {:?} with preimage {:?}",
						payment.amt_msat,
						if let Some(fee) = fee_paid_msat {
							format!(" (fee {} msat)", fee)
						} else {
							"".to_string()
						},
						hex_utils::hex_str(&payment_hash.0),
						hex_utils::hex_str(&payment_preimage.0)
					);
					print!("> ");
					io::stdout().flush().unwrap();
				}
			}
			print!("> ");
		}
		Event::OpenChannelRequest { .. } => {
			// Unreachable, we don't set manually_accept_inbound_channels
		}
		Event::PaymentPathSuccessful { .. } => {}
		Event::PaymentPathFailed { .. } => {}
		Event::ProbeSuccessful { .. } => {}
		Event::ProbeFailed { .. } => {}
		Event::PaymentFailed { payment_hash, .. } => {
			println!(
				"\nEVENT: Failed to send payment to payment hash {:?}: exhausted payment retry attempts",
				hex_utils::hex_str(&payment_hash.0)
			);
			print!("> ");
			io::stdout().flush().unwrap();

			let mut payments = outbound_payments.lock().unwrap();
			if payments.contains_key(&payment_hash) {
				let payment = payments.get_mut(&payment_hash).unwrap();
				payment.status = HTLCStatus::Failed;
			}
		}
		Event::PaymentForwarded {
			prev_channel_id,
			next_channel_id,
			fee_earned_msat,
			claim_from_onchain_tx,
		} => {
			let read_only_network_graph = network_graph.read_only();
			let nodes = read_only_network_graph.nodes();
			let channels = channel_manager.list_channels();

			let node_str = |channel_id: &Option<[u8; 32]>| match channel_id {
				None => String::new(),
				Some(channel_id) => match channels.iter().find(|c| c.channel_id == *channel_id) {
					None => String::new(),
					Some(channel) => {
						match nodes.get(&NodeId::from_pubkey(&channel.counterparty.node_id)) {
							None => "private node".to_string(),
							Some(node) => match &node.announcement_info {
								None => "unnamed node".to_string(),
								Some(announcement) => {
									format!("node {}", announcement.alias)
								}
							},
						}
					}
				},
			};
			let channel_str = |channel_id: &Option<[u8; 32]>| {
				channel_id
					.map(|channel_id| format!(" with channel {}", hex_utils::hex_str(&channel_id)))
					.unwrap_or_default()
			};
			let from_prev_str =
				format!(" from {}{}", node_str(prev_channel_id), channel_str(prev_channel_id));
			let to_next_str =
				format!(" to {}{}", node_str(next_channel_id), channel_str(next_channel_id));

			let from_onchain_str = if *claim_from_onchain_tx {
				"from onchain downstream claim"
			} else {
				"from HTLC fulfill message"
			};
			if let Some(fee_earned) = fee_earned_msat {
				println!(
					"\nEVENT: Forwarded payment{}{}, earning {} msat {}",
					from_prev_str, to_next_str, fee_earned, from_onchain_str
				);
			} else {
				println!(
					"\nEVENT: Forwarded payment{}{}, claiming onchain {}",
					from_prev_str, to_next_str, from_onchain_str
				);
			}
			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::HTLCHandlingFailed { .. } => {}
		Event::PendingHTLCsForwardable { time_forwardable } => {
			let forwarding_channel_manager = channel_manager.clone();
			let min = time_forwardable.as_millis() as u64;
			tokio::spawn(async move {
				let millis_to_sleep = thread_rng().gen_range(min, min * 5) as u64;
				tokio::time::sleep(Duration::from_millis(millis_to_sleep)).await;
				forwarding_channel_manager.process_pending_htlc_forwards();
			});
		}
		Event::SpendableOutputs { outputs } => {
			let secp_ctx = Secp256k1::new();
			let output_descriptors = &outputs.iter().map(|a| a).collect::<Vec<_>>();
			let tx_feerate =
				bitcoind_client.get_est_sat_per_1000_weight(ConfirmationTarget::Normal);
			let mut rgb_client = rgb_node_client.lock().unwrap();

			for outp in output_descriptors {
				let outpoint = match outp {
					SpendableOutputDescriptor::StaticPaymentOutput(descriptor) => {
						descriptor.outpoint
					}
					SpendableOutputDescriptor::DelayedPaymentOutput(descriptor) => {
						descriptor.outpoint
					}
					SpendableOutputDescriptor::StaticOutput { ref outpoint, output: _ } => {
						*outpoint
					}
				};

				let consignment_path = format!("{ldk_data_dir}/consignment_{}", outpoint.txid);
				let consignment = StateTransfer::strict_file_load(&consignment_path).expect("ok");

				let reveal = Reveal {
					blinding_factor: 777,
					outpoint: OutPoint { txid: outpoint.txid, vout: outpoint.index as u32 },
					close_method: CloseMethod::OpretFirst,
					witness_vout: true,
				};
				let status = rgb_client
					.consume_transfer(consignment.clone(), true, Some(reveal), |_| ())
					.expect("valid consume tranfer");
				if !matches!(status, ContractValidity::Valid) {
					println!("error consuming transfer");
				}

				let contract_id = consignment.contract_id();
				let wallet = wallet_arc.lock().unwrap();
				let address = wallet
					.get_address(bdk::wallet::AddressIndex::New)
					.expect("valid address")
					.address;
				let rgb_inputs: Vec<OutPoint> =
					vec![OutPoint { txid: outpoint.txid, vout: outpoint.index as u32 }];
				let rgb_change = vec![];
				let input_outpoints_bt: BTreeSet<OutPoint> =
					rgb_inputs.clone().into_iter().collect();
				let bundle = &consignment
					.anchored_bundles()
					.find(|ab| ab.0.txid == outpoint.txid)
					.expect("found bundle for closing tx")
					.1;
				let transition = bundle.clone().into_revealed_iter().next().unwrap().0;
				let owned_rights = transition.owned_rights();
				let typed_assignments: Vec<&TypedAssignments> =
					owned_rights.iter().map(|or| or.1).collect();
				let value_assignments: Vec<Assignment<PedersenStrategy>> =
					typed_assignments.iter().flat_map(|ta| ta.to_value_assignments()).collect();
				let assignment = value_assignments
					.iter()
					.find(|va| va.revealed_seal().unwrap().vout == outpoint.index as u32)
					.unwrap();
				let amt_rgb = assignment.as_revealed_state().unwrap().value;
				let beneficiaries: EndpointValueMap = bmap![
					SealEndpoint::WitnessVout {
						method: CloseMethod::OpretFirst,
						vout: 0,
						blinding: 777,
					} => amt_rgb
				];

				let (tx, consignment) = match outp {
					SpendableOutputDescriptor::StaticPaymentOutput(descriptor) => {
						let signer = keys_manager.derive_channel_keys(
							descriptor.channel_value_satoshis,
							&descriptor.channel_keys_id,
						);
						let intermediate_wallet = get_bdk_wallet_seckey(
							format!("{ldk_data_dir}/intermediate"),
							network,
							signer.payment_key,
						);
						sync_wallet(&intermediate_wallet, electrum_url.clone());
						let mut builder = intermediate_wallet.build_tx();
						builder
							.add_utxos(&rgb_inputs)
							.expect("valid utxos")
							.fee_rate(FeeRate::from_sat_per_vb(FEE_RATE))
							.manually_selected_only()
							.drain_to(address.script_pubkey());
						let psbt = builder.finish().expect("valid psbt finish").0;

						let (mut psbt, consignment) = rgb_client.send_rgb(
							contract_id,
							psbt,
							input_outpoints_bt,
							beneficiaries,
							rgb_change,
						);

						intermediate_wallet
							.sign(&mut psbt, SignOptions::default())
							.expect("able to sign");

						(psbt.extract_tx(), consignment)
					}
					SpendableOutputDescriptor::DelayedPaymentOutput(descriptor) => {
						let signer = keys_manager.derive_channel_keys(
							descriptor.channel_value_satoshis,
							&descriptor.channel_keys_id,
						);
						let input = vec![TxIn {
							previous_output: descriptor.outpoint.into_bitcoin_outpoint(),
							script_sig: Script::new(),
							sequence: Sequence(descriptor.to_self_delay as u32),
							witness: Witness::new(),
						}];
						let witness_weight = DelayedPaymentOutputDescriptor::MAX_WITNESS_LENGTH;
						let input_value = descriptor.output.value;
						let output = vec![];
						let mut spend_tx =
							Transaction { version: 2, lock_time: PackedLockTime(0), input, output };
						let _expected_max_weight =
							lightning::util::transaction_utils::maybe_add_change_output(
								&mut spend_tx,
								input_value,
								witness_weight,
								tx_feerate,
								address.script_pubkey(),
							)
							.expect("can add change");

						let mut psbt = Psbt::with(spend_tx.clone(), PsbtVersion::V0)
							.expect("valid transaction");
						let (psbt, consignment) = rgb_client.send_rgb_internal(
							contract_id,
							&mut psbt,
							input_outpoints_bt,
							beneficiaries,
							rgb_change,
						);

						let mut spend_tx = psbt.extract_tx();
						let input_idx = 0;
						let witness_vec = signer
							.sign_dynamic_p2wsh_input(&spend_tx, input_idx, descriptor, &secp_ctx)
							.expect("possible dynamic sign");
						spend_tx.input[input_idx].witness = Witness::from_vec(witness_vec);

						(spend_tx, consignment)
					}
					SpendableOutputDescriptor::StaticOutput { outpoint: _, ref output } => {
						let derivation_idx =
							if output.script_pubkey == keys_manager.destination_script {
								1
							} else {
								2
							};
						let secret = keys_manager
							.master_key
							.ckd_priv(
								&secp_ctx,
								ChildNumber::from_hardened_idx(derivation_idx).unwrap(),
							)
							.unwrap();
						let intermediate_wallet = get_bdk_wallet_seckey(
							format!("{ldk_data_dir}/intermediate"),
							network,
							secret.private_key,
						);
						sync_wallet(&intermediate_wallet, electrum_url.clone());
						let mut builder = intermediate_wallet.build_tx();
						builder
							.add_utxos(&rgb_inputs)
							.expect("valid utxos")
							.fee_rate(FeeRate::from_sat_per_vb(FEE_RATE))
							.manually_selected_only()
							.drain_to(address.script_pubkey());
						let psbt = builder.finish().expect("valid psbt finish").0;

						let (mut psbt, consignment) = rgb_client.send_rgb(
							contract_id,
							psbt,
							input_outpoints_bt,
							beneficiaries,
							rgb_change,
						);

						intermediate_wallet
							.sign(&mut psbt, SignOptions::default())
							.expect("able to sign");

						(psbt.extract_tx(), consignment)
					}
				};

				broadcast_tx(&tx, electrum_url.clone());
				sync_wallet(&wallet, electrum_url.clone());

				let reveal = Reveal {
					blinding_factor: 777,
					outpoint: OutPoint { txid: tx.txid(), vout: 0 },
					close_method: CloseMethod::OpretFirst,
					witness_vout: true,
				};
				let _status = rgb_client
					.consume_transfer(consignment, true, Some(reveal), |_| ())
					.expect("valid consume tranfer");
			}

			drop(rgb_client);

			println!("Event::SpendableOutputs complete");
		}
		Event::ChannelReady {
			ref channel_id,
			user_channel_id: _,
			ref counterparty_node_id,
			channel_type: _,
		} => {
			println!(
				"\nEVENT: Channel {} with peer {} is ready to be used!",
				hex_utils::hex_str(channel_id),
				hex_utils::hex_str(&counterparty_node_id.serialize()),
			);

			let funding_consignment_path =
				format!("{}/consignment_{}", ldk_data_dir, hex::encode(&channel_id));
			if PathBuf::from(&funding_consignment_path).exists() {
				// there is an RGB change
				let funding_consignment =
					StateTransfer::strict_file_load(&funding_consignment_path).expect("ok");

				let funding_txid = funding_consignment.anchored_bundles().last().unwrap().0.txid;

				let reveal = Reveal {
					blinding_factor: 777,
					outpoint: OutPoint { txid: funding_txid, vout: 1 },
					close_method: CloseMethod::OpretFirst,
					witness_vout: true,
				};
				let mut rgb_client = rgb_node_client.lock().unwrap();
				let _status = rgb_client
					.consume_transfer(funding_consignment, true, Some(reveal), |_| ())
					.expect("valid consume tranfer");
			}

			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::ChannelClosed { channel_id, reason, user_channel_id: _ } => {
			println!(
				"\nEVENT: Channel {} closed due to: {:?}",
				hex_utils::hex_str(channel_id),
				reason
			);

			print!("> ");
			io::stdout().flush().unwrap();
		}
		Event::DiscardFunding { .. } => {
			// A "real" node should probably "lock" the UTXOs spent in funding transactions until
			// the funding transaction either confirms, or this event is generated.
		}
		Event::HTLCIntercepted { is_swap, payment_hash, intercept_id, inbound_amount_msat, expected_outbound_amount_msat, inbound_rgb_amount, expected_outbound_rgb_amount, requested_next_hop_scid, prev_short_channel_id } => {
			if !is_swap {
				channel_manager.fail_intercepted_htlc(*intercept_id).unwrap();
			}

			let ldk_data_dir_path = PathBuf::from(ldk_data_dir.clone());
			let get_rgb_info = |channel_id| {
				let info_file_path = ldk_data_dir_path.join(hex::encode(channel_id));
				if info_file_path.exists() {
					let (rgb_info, _) = get_rgb_channel_info(channel_id, &ldk_data_dir_path);
					Some((
						rgb_info.contract_id,
						rgb_info.local_rgb_amount,
						rgb_info.remote_rgb_amount,
					))
				} else {
					None
				}
			};

			let inbound_channel = channel_manager.list_channels().into_iter().find(|details| details.short_channel_id == Some(*prev_short_channel_id)).expect("Should always be a valid channel");
			let outbound_channel = channel_manager.list_channels().into_iter().find(|details| details.short_channel_id == Some(*requested_next_hop_scid)).expect("Should always be a valid channel");

			let inbound_rgb_info = get_rgb_info(&inbound_channel.channel_id);
			let outbound_rgb_info = get_rgb_info(&outbound_channel.channel_id);

			println!("EVENT: Requested swap with params inbound_msat={} outbound_msat={} inbound_rgb={:?} outbound_rgb={:?} inbound_contract_id={:?}, outbound_contract_id={:?}", inbound_amount_msat, expected_outbound_amount_msat, inbound_rgb_amount, expected_outbound_rgb_amount, inbound_rgb_info.map(|i| i.0), outbound_rgb_info.map(|i| i.0));
			
			let mut trades_lock = whitelisted_trades.lock().unwrap();
			let (whitelist_contract_id, whitelist_swap_type) = match trades_lock.get(payment_hash) {
				None => {
					println!("ERROR: rejecting non-whitelisted swap");
					channel_manager.fail_intercepted_htlc(*intercept_id).unwrap();
					return;
				},
				Some(x) => x,
			};

			match whitelist_swap_type {
				SwapType::BuyAsset { amount_rgb, amount_msats } => {
					let net_msat_diff = expected_outbound_amount_msat.saturating_sub(*inbound_amount_msat);

					if *inbound_rgb_amount != Some(*amount_rgb) ||
						inbound_rgb_info.map(|x| x.0) != Some(*whitelist_contract_id) ||
						net_msat_diff != *amount_msats ||
						outbound_rgb_info.is_some()
						{
						println!("ERROR: swap doesn't match the whitelisted info, rejecting it");
						channel_manager.fail_intercepted_htlc(*intercept_id).unwrap();
						return;
					}
				}
				SwapType::SellAsset { amount_rgb, amount_msats } => {
					let net_msat_diff = inbound_amount_msat.saturating_sub(*expected_outbound_amount_msat);

					if *expected_outbound_rgb_amount != Some(*amount_rgb) ||
						outbound_rgb_info.map(|x| x.0) != Some(*whitelist_contract_id) ||
						net_msat_diff != *amount_msats ||
						inbound_rgb_info.is_some()
						{
						println!("ERROR: swap doesn't match the whitelisted info, rejecting it");
						channel_manager.fail_intercepted_htlc(*intercept_id).unwrap();
						return;
					}
				}
			}

			println!("Swap is whitelisted, forwarding the htlc...");
			trades_lock.remove(payment_hash);

			channel_manager.forward_intercepted_htlc(*intercept_id, channelmanager::NextHopForward::ShortChannelId(*requested_next_hop_scid), *expected_outbound_amount_msat, *expected_outbound_rgb_amount).expect("Forward should be valid");
		}
	}
}

async fn start_ldk() {
	let args = match args::parse_startup_args() {
		Ok(user_args) => user_args,
		Err(()) => return,
	};

	// Initialize the LDK data directory if necessary.
	let ldk_data_dir = format!("{}/.ldk", args.ldk_storage_dir_path);
	let ldk_data_dir_path = PathBuf::from(&ldk_data_dir);
	fs::create_dir_all(ldk_data_dir.clone()).unwrap();

	let blinded_dir = ldk_data_dir_path.join("blinded_utxos");
	fs::create_dir_all(blinded_dir).expect("successful directory creation");

	// ## Setup
	// Step 1: Initialize the Logger
	let logger = Arc::new(FilesystemLogger::new(ldk_data_dir.clone()));

	// Initialize our bitcoind client.
	let bitcoind_client = match BitcoindClient::new(
		args.bitcoind_rpc_host.clone(),
		args.bitcoind_rpc_port,
		args.bitcoind_rpc_username.clone(),
		args.bitcoind_rpc_password.clone(),
		tokio::runtime::Handle::current(),
		Arc::clone(&logger),
	)
	.await
	{
		Ok(client) => Arc::new(client),
		Err(e) => {
			println!("Failed to connect to bitcoind client: {}", e);
			return;
		}
	};

	// Check that the bitcoind we've connected to is running the network we expect
	let bitcoind_chain = bitcoind_client.get_blockchain_info().await.chain;
	if bitcoind_chain
		!= match args.network {
			bitcoin::Network::Bitcoin => "main",
			bitcoin::Network::Testnet => "test",
			bitcoin::Network::Regtest => "regtest",
			bitcoin::Network::Signet => "signet",
		} {
		println!(
			"Chain argument ({}) didn't match bitcoind chain ({})",
			args.network, bitcoind_chain
		);
		return;
	}

	// RGB setup
	let (electrum_url, proxy_url, rgb_network) = match args.network {
		bitcoin::Network::Testnet => (ELECTRUM_URL_TESTNET, PROXY_URL_TESTNET, Chain::Testnet3),
		bitcoin::Network::Regtest => (
			ELECTRUM_URL_REGTEST,
			PROXY_URL_REGTEST,
			Chain::Regtest(BlockHash::from_slice(GENESIS_HASH_REGTEST).unwrap()),
		),
		_ => {
			println!("ERROR: PoC does not support selected network");
			return;
		}
	};
	let rgb_node_client =
		Arc::new(Mutex::new(get_rgb_node_client(args.rgb_node_port, rgb_network.clone())));
	fs::write(format!("{ldk_data_dir}/rgb_node_port"), args.rgb_node_port.to_string())
		.expect("able to write");
	fs::write(format!("{ldk_data_dir}/rgb_node_network"), rgb_network.to_string())
		.expect("able to write");
	let rest_client = RestClient::builder()
		.timeout(Duration::from_secs(PROXY_TIMEOUT as u64))
		.build()
		.expect("valid proxy");
	let proxy_client = Arc::new(rest_client);

	// ## Setup
	// Step 2: Initialize the FeeEstimator

	// BitcoindClient implements the FeeEstimator trait, so it'll act as our fee estimator.
	let fee_estimator = bitcoind_client.clone();

	// Step 3: Initialize the BroadcasterInterface

	// BitcoindClient implements the BroadcasterInterface trait, so it'll act as our transaction
	// broadcaster.
	let broadcaster = bitcoind_client.clone();

	// Step 4: Initialize Persist
	let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

	// Step 5: Initialize the ChainMonitor
	let chain_monitor: Arc<ChainMonitor> = Arc::new(chainmonitor::ChainMonitor::new(
		None,
		broadcaster.clone(),
		logger.clone(),
		fee_estimator.clone(),
		persister.clone(),
	));

	// Step 6: Initialize the KeysManager

	// The key seed that we use to derive the node privkey (that corresponds to the node pubkey) and
	// other secret key material.
	let keys_seed_path = format!("{}/keys_seed", ldk_data_dir.clone());
	let keys_seed = if let Ok(seed) = fs::read(keys_seed_path.clone()) {
		assert_eq!(seed.len(), 32);
		let mut key = [0; 32];
		key.copy_from_slice(&seed);
		key
	} else {
		let mut key = [0; 32];
		thread_rng().fill_bytes(&mut key);
		match File::create(keys_seed_path.clone()) {
			Ok(mut f) => {
				f.write_all(&key).expect("Failed to write node keys seed to disk");
				f.sync_all().expect("Failed to sync node keys seed to disk");
			}
			Err(e) => {
				println!("ERROR: Unable to create keys seed file {}: {}", keys_seed_path, e);
				return;
			}
		}
		key
	};
	let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();

	let master_xprv = ExtendedPrivKey::new_master(Network::Testnet, &keys_seed).unwrap();
	let secp = Secp256k1::new();
	let xprv: ExtendedPrivKey =
		master_xprv.ckd_priv(&secp, ChildNumber::Hardened { index: 535 }).unwrap();
	let ldk_seed: [u8; 32] = xprv.private_key.secret_bytes();

	let keys_manager = Arc::new(KeysManager::new(
		&ldk_seed,
		cur.as_secs(),
		cur.subsec_nanos(),
		ldk_data_dir_path.clone(),
	));
	let wallet = Arc::new(Mutex::new(get_bdk_wallet(
		ldk_data_dir.clone(),
		keys_manager.master_key,
		args.network,
	)));

	// Step 7: Read ChannelMonitor state from disk
	let mut channelmonitors =
		persister.read_channelmonitors(keys_manager.clone(), keys_manager.clone()).unwrap();

	// Step 8: Poll for the best chain tip, which may be used by the channel manager & spv client
	let polled_chain_tip = init::validate_best_block_header(bitcoind_client.as_ref())
		.await
		.expect("Failed to fetch best block header and best block");

	// Step 9: Initialize routing ProbabilisticScorer
	let network_graph_path = format!("{}/network_graph", ldk_data_dir.clone());
	let network_graph =
		Arc::new(disk::read_network(Path::new(&network_graph_path), args.network, logger.clone()));

	let scorer_path = format!("{}/scorer", ldk_data_dir.clone());
	let scorer = Arc::new(Mutex::new(disk::read_scorer(
		Path::new(&scorer_path),
		Arc::clone(&network_graph),
		Arc::clone(&logger),
	)));

	// Step 10: Create Router
	let router = Arc::new(DefaultRouter::new(
		network_graph.clone(),
		logger.clone(),
		keys_manager.get_secure_random_bytes(),
		scorer.clone(),
	));

	// Step 11: Initialize the ChannelManager
	let mut user_config = UserConfig::default();
	user_config.channel_handshake_limits.force_announced_channel_preference = false;
	user_config.accept_intercept_htlcs = true;
	let mut restarting_node = true;
	let (channel_manager_blockhash, channel_manager) = {
		if let Ok(mut f) = fs::File::open(format!("{}/manager", ldk_data_dir.clone())) {
			let mut channel_monitor_mut_references = Vec::new();
			for (_, channel_monitor) in channelmonitors.iter_mut() {
				channel_monitor_mut_references.push(channel_monitor);
			}
			let read_args = ChannelManagerReadArgs::new(
				keys_manager.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				router.clone(),
				logger.clone(),
				user_config,
				channel_monitor_mut_references,
				ldk_data_dir_path.clone(),
			);
			<(BlockHash, ChannelManager)>::read(&mut f, read_args).unwrap()
		} else {
			// We're starting a fresh node.
			restarting_node = false;

			let polled_best_block = polled_chain_tip.to_best_block();
			let polled_best_block_hash = polled_best_block.block_hash();
			let chain_params =
				ChainParameters { network: args.network, best_block: polled_best_block };
			let fresh_channel_manager = channelmanager::ChannelManager::new(
				fee_estimator.clone(),
				chain_monitor.clone(),
				broadcaster.clone(),
				router.clone(),
				logger.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				keys_manager.clone(),
				user_config,
				chain_params,
				ldk_data_dir_path.clone(),
			);
			(polled_best_block_hash, fresh_channel_manager)
		}
	};

	// initialize RGB UTXOs file on first run
	if !restarting_node {
		let rgb_utxos = RgbUtxos { utxos: vec![] };
		let rgb_utxos_path = format!("{}/rgb_utxos", ldk_data_dir);
		let serialized_utxos = serde_json::to_string(&rgb_utxos).expect("valid rgb utxos");
		fs::write(rgb_utxos_path, serialized_utxos).expect("able to write rgb utxos file");
	}

	// Step 12: Sync ChannelMonitors and ChannelManager to chain tip
	let mut chain_listener_channel_monitors = Vec::new();
	let mut cache = UnboundedCache::new();
	let chain_tip = if restarting_node {
		let mut chain_listeners = vec![(
			channel_manager_blockhash,
			&channel_manager as &(dyn chain::Listen + Send + Sync),
		)];

		for (blockhash, channel_monitor) in channelmonitors.drain(..) {
			let outpoint = channel_monitor.get_funding_txo().0;
			chain_listener_channel_monitors.push((
				blockhash,
				(channel_monitor, broadcaster.clone(), fee_estimator.clone(), logger.clone()),
				outpoint,
			));
		}

		for monitor_listener_info in chain_listener_channel_monitors.iter_mut() {
			chain_listeners.push((
				monitor_listener_info.0,
				&monitor_listener_info.1 as &(dyn chain::Listen + Send + Sync),
			));
		}

		init::synchronize_listeners(
			bitcoind_client.as_ref(),
			args.network,
			&mut cache,
			chain_listeners,
		)
		.await
		.unwrap()
	} else {
		polled_chain_tip
	};

	// Step 13: Give ChannelMonitors to ChainMonitor
	for item in chain_listener_channel_monitors.drain(..) {
		let channel_monitor = item.1 .0;
		let funding_outpoint = item.2;
		assert_eq!(
			chain_monitor.watch_channel(funding_outpoint, channel_monitor),
			ChannelMonitorUpdateStatus::Completed
		);
	}

	// Step 14: Optional: Initialize the P2PGossipSync
	let gossip_sync = Arc::new(P2PGossipSync::new(
		Arc::clone(&network_graph),
		None::<Arc<BitcoindClient>>,
		logger.clone(),
	));

	// Step 15: Initialize the PeerManager
	let channel_manager: Arc<ChannelManager> = Arc::new(channel_manager);
	let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
		Arc::clone(&keys_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&logger),
		IgnoringMessageHandler {},
	));
	let mut ephemeral_bytes = [0; 32];
	let current_time = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
	rand::thread_rng().fill_bytes(&mut ephemeral_bytes);
	let lightning_msg_handler = MessageHandler {
		chan_handler: channel_manager.clone(),
		route_handler: gossip_sync.clone(),
		onion_message_handler: onion_messenger.clone(),
	};
	let peer_manager: Arc<PeerManager> = Arc::new(PeerManager::new(
		lightning_msg_handler,
		current_time.try_into().unwrap(),
		&ephemeral_bytes,
		logger.clone(),
		IgnoringMessageHandler {},
		Arc::clone(&keys_manager),
	));

	// ## Running LDK
	// Step 16: Initialize networking

	let peer_manager_connection_handler = peer_manager.clone();
	let listening_port = args.ldk_peer_listening_port;
	let stop_listen_connect = Arc::new(AtomicBool::new(false));
	let stop_listen = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let listener = tokio::net::TcpListener::bind(format!("[::]:{}", listening_port))
			.await
			.expect("Failed to bind to listen port - is something else already listening on it?");
		loop {
			let peer_mgr = peer_manager_connection_handler.clone();
			let tcp_stream = listener.accept().await.unwrap().0;
			if stop_listen.load(Ordering::Acquire) {
				return;
			}
			tokio::spawn(async move {
				lightning_net_tokio::setup_inbound(
					peer_mgr.clone(),
					tcp_stream.into_std().unwrap(),
				)
				.await;
			});
		}
	});

	// Step 17: Connect and Disconnect Blocks
	let channel_manager_listener = channel_manager.clone();
	let chain_monitor_listener = chain_monitor.clone();
	let bitcoind_block_source = bitcoind_client.clone();
	let network = args.network;
	tokio::spawn(async move {
		let chain_poller = poll::ChainPoller::new(bitcoind_block_source.as_ref(), network);
		let chain_listener = (chain_monitor_listener, channel_manager_listener);
		let mut spv_client = SpvClient::new(chain_tip, chain_poller, &mut cache, &chain_listener);
		loop {
			spv_client.poll_best_tip().await.unwrap();
			tokio::time::sleep(Duration::from_secs(1)).await;
		}
	});

	let whitelisted_trades = Arc::new(Mutex::new(HashMap::new()));

	// Step 18: Handle LDK Events
	let channel_manager_event_listener = channel_manager.clone();
	let keys_manager_listener = keys_manager.clone();
	// TODO: persist payment info to disk
	let inbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
	let outbound_payments: PaymentInfoStorage = Arc::new(Mutex::new(HashMap::new()));
	let inbound_pmts_for_events = inbound_payments.clone();
	let outbound_pmts_for_events = outbound_payments.clone();
	let network = args.network;
	let bitcoind_rpc = bitcoind_client.clone();
	let network_graph_events = network_graph.clone();
	let handle = tokio::runtime::Handle::current();
	let ldk_data_dir_copy = ldk_data_dir.clone();
	let rgb_node_client_copy = rgb_node_client.clone();
	let proxy_client_copy = proxy_client.clone();
	let wallet_copy = wallet.clone();
	let whitelisted_trades_copy = whitelisted_trades.clone();
	let event_handler = move |event: Event| {
		handle.block_on(handle_ldk_events(
			&channel_manager_event_listener,
			&bitcoind_rpc,
			&network_graph_events,
			&keys_manager_listener,
			&inbound_pmts_for_events,
			&outbound_pmts_for_events,
			network,
			&event,
			ldk_data_dir_copy.clone(),
			rgb_node_client_copy.clone(),
			proxy_client_copy.clone(),
			proxy_url,
			wallet_copy.clone(),
			electrum_url.to_string(),
			&whitelisted_trades_copy,
		));
	};

	// Step 19: Persist ChannelManager and NetworkGraph
	let persister = Arc::new(FilesystemPersister::new(ldk_data_dir.clone()));

	// Step 20: Background Processing
	let background_processor = BackgroundProcessor::start(
		persister,
		event_handler,
		chain_monitor.clone(),
		channel_manager.clone(),
		GossipSync::p2p(gossip_sync.clone()),
		peer_manager.clone(),
		logger.clone(),
		Some(scorer.clone()),
	);

	// Regularly reconnect to channel peers.
	let connect_cm = Arc::clone(&channel_manager);
	let connect_pm = Arc::clone(&peer_manager);
	let peer_data_path = format!("{}/channel_peer_data", ldk_data_dir.clone());
	let stop_connect = Arc::clone(&stop_listen_connect);
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(Duration::from_secs(1));
		loop {
			interval.tick().await;
			match disk::read_channel_peer_data(Path::new(&peer_data_path)) {
				Ok(info) => {
					let peers = connect_pm.get_peer_node_ids();
					for node_id in connect_cm
						.list_channels()
						.iter()
						.map(|chan| chan.counterparty.node_id)
						.filter(|id| !peers.iter().any(|(pk, _)| id == pk))
					{
						if stop_connect.load(Ordering::Acquire) {
							return;
						}
						for (pubkey, peer_addr) in info.iter() {
							if *pubkey == node_id {
								let _ = cli::do_connect_peer(
									*pubkey,
									peer_addr.clone(),
									Arc::clone(&connect_pm),
								)
								.await;
							}
						}
					}
				}
				Err(e) => println!("ERROR: errored reading channel peer info from disk: {:?}", e),
			}
		}
	});

	// Regularly broadcast our node_announcement. This is only required (or possible) if we have
	// some public channels, and is only useful if we have public listen address(es) to announce.
	// In a production environment, this should occur only after the announcement of new channels
	// to avoid churn in the global network graph.
	let peer_man = Arc::clone(&peer_manager);
	let network = args.network;
	if !args.ldk_announced_listen_addr.is_empty() {
		tokio::spawn(async move {
			let mut interval = tokio::time::interval(Duration::from_secs(60));
			loop {
				interval.tick().await;
				peer_man.broadcast_node_announcement(
					[0; 3],
					args.ldk_announced_node_name,
					args.ldk_announced_listen_addr.clone(),
				);
			}
		});
	}

	// Start the CLI.
	cli::poll_for_user_input(
		Arc::clone(&peer_manager),
		Arc::clone(&channel_manager),
		Arc::clone(&keys_manager),
		Arc::clone(&network_graph),
		Arc::clone(&onion_messenger),
		Arc::clone(&router),
		inbound_payments,
		outbound_payments,
		ldk_data_dir.clone(),
		network,
		Arc::clone(&logger),
		Arc::clone(&bitcoind_client),
		Arc::clone(&rgb_node_client),
		proxy_client.clone(),
		proxy_url,
		wallet.clone(),
		electrum_url.to_string(),
		whitelisted_trades,
	)
	.await;

	// Disconnect our peers and stop accepting new connections. This ensures we don't continue
	// updating our channel data after we've stopped the background processor.
	stop_listen_connect.store(true, Ordering::Release);
	peer_manager.disconnect_all_peers();

	// Stop the background processor.
	background_processor.stop().unwrap();
}

#[tokio::main]
pub async fn main() {
	#[cfg(not(target_os = "windows"))]
	{
		// Catch Ctrl-C with a dummy signal handler.
		unsafe {
			let mut new_action: libc::sigaction = core::mem::zeroed();
			let mut old_action: libc::sigaction = core::mem::zeroed();

			extern "C" fn dummy_handler(
				_: libc::c_int, _: *const libc::siginfo_t, _: *const libc::c_void,
			) {
			}

			new_action.sa_sigaction = dummy_handler as libc::sighandler_t;
			new_action.sa_flags = libc::SA_SIGINFO;

			libc::sigaction(
				libc::SIGINT,
				&new_action as *const libc::sigaction,
				&mut old_action as *mut libc::sigaction,
			);
		}
	}

	start_ldk().await;
}
