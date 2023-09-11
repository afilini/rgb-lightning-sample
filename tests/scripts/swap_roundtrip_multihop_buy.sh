#!/usr/bin/env bash

source tests/common.sh


get_node_ids

# create RGB UTXOs
create_utxos 1
create_utxos 2
create_utxos 3

# issue asset
issue_asset

# send assets
blind 2
send_assets 1 400
asset_balance 1 600

# open channel
open_colored_channel 1 2 "$NODE2_PORT" "$NODE2_ID" 500
channel12_id="$CHANNEL_ID"
list_channels 1
list_channels 2
asset_balance 1 100

refresh 2
asset_balance 2 400

# open channel
open_colored_channel 2 3 "$NODE3_PORT" "$NODE3_ID" 300 1
channel23_id="$CHANNEL_ID"
list_channels 2 2
list_channels 3
asset_balance 2 100

open_vanilla_channel 2 1 "$NODE1_PORT" "$NODE1_ID" 16777215
list_channels 2 2
list_channels 1 2

open_vanilla_channel 3 2 "$NODE2_PORT" "$NODE2_ID" 16777215
list_channels 3 2
list_channels 2 4

sleep 5
maker_init 1 2 "buy" 90
taker 3
taker_list 3 1
maker_list 1 1
maker_execute 1

exit 0
