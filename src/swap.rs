use std::convert::TryInto;

use lightning::ln::PaymentHash;
use rgb::contract::ContractId;

use crate::hex_utils;

#[derive(Debug)]
pub enum SwapType {
	BuyAsset { amount_rgb: u64, amount_msats: u64 },
	SellAsset { amount_rgb: u64, amount_msats: u64 },
}

impl SwapType {
	pub fn opposite(self) -> Self {
		match self {
			SwapType::BuyAsset { amount_rgb, amount_msats } => {
				SwapType::SellAsset { amount_rgb, amount_msats }
			}
			SwapType::SellAsset { amount_rgb, amount_msats } => {
				SwapType::BuyAsset { amount_rgb, amount_msats }
			}
		}
	}

	pub fn is_buy(&self) -> bool {
		matches!(self, SwapType::BuyAsset { .. })
	}

	pub fn amount_msats(&self) -> u64 {
		match self {
			SwapType::BuyAsset { amount_msats, .. } | SwapType::SellAsset { amount_msats, .. } => {
				*amount_msats
			}
		}
	}

	pub fn side(&self) -> &'static str {
		match self {
			SwapType::BuyAsset { .. } => "buy",
			SwapType::SellAsset { .. } => "sell",
		}
	}
}

#[derive(Debug)]
pub struct SwapString {
	pub asset_id: ContractId,
	pub swap_type: SwapType,
	pub payment_hash: PaymentHash,
}

impl std::str::FromStr for SwapString {
	type Err = &'static str;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let mut iter = s.split(":");
		let amount = iter.next();
		let asset_id = iter.next();
		let side = iter.next();
		let price = iter.next();
		let payment_hash = iter.next();

		if payment_hash.is_none() || iter.next().is_some() {
			return Err("Wrong number of parts");
		}

		let amount = amount.unwrap().parse::<u64>();
		let asset_id = ContractId::from_str(asset_id.unwrap());
		let price = price.unwrap().parse::<u64>();
		let payment_hash = hex_utils::to_vec(payment_hash.unwrap())
			.and_then(|vec| vec.try_into().ok())
			.map(|slice| PaymentHash(slice));

		if amount.is_err() || asset_id.is_err() || price.is_err() || payment_hash.is_none() {
			return Err("Unable to parse parts");
		}

		let amount = amount.unwrap();
		let asset_id = asset_id.unwrap();
		let price = price.unwrap();
		let payment_hash = payment_hash.unwrap();

		let swap_type = match side {
			Some("buy") => SwapType::BuyAsset { amount_rgb: amount, amount_msats: amount * price },
			Some("sell") => {
				SwapType::SellAsset { amount_rgb: amount, amount_msats: amount * price }
			}
			_ => {
				return Err("Invalid swap type");
			}
		};

		Ok(SwapString { asset_id, swap_type, payment_hash })
	}
}