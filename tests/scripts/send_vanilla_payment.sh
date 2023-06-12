#!/usr/bin/env bash

source tests/common.sh


get_node_ids

get_address 1
fund_address $address
mine 1
sync_wallet 1

# open channel
open_vanilla_channel 1 2 "$NODE2_PORT" "$node2_id" 16777215
list_channels 1
list_channels 2

# get invoice
get_vanilla_invoice 2 3000000

# send payment
send_payment 1 2 "$invoice"
list_channels 1
list_channels 2
list_payments 1
list_payments 2