#!/use/bin/env bash


# BITCOIN_CLI and TMUX_CMD expected to be set in the environment
DEBUG=${DEBUG:-0}

T_1=60
T_2=120
T_5=300
T_10=600
T_15=900

NODE1_PORT=9735
NODE2_PORT=9736
NODE3_PORT=9737

# shell colors
C1='\033[0;32m' # green
C2='\033[0;33m' # orange
C3='\033[0;34m' # blue
NC='\033[0m'    # No Color


_die() {
    timestamp
    _latest_logs
    echo
    echo "ERR: $*"
    exit 1
}

_tit() {
    printf "\n${C3}========[ %s ]${NC}\n" "$@"
}

_subtit() {
    printf "${C2}==== %s${NC}\n" "$@"
}

_out() {
    printf "${C1}--> %s${NC}\n" "$@"
}

_get_last_text() {
    local pane pattern
    pane="$1"
    pattern="$2"
    $TMUX_CMD capture-pane -ep -t "$pane" -S -20 \
        | tac | sed -n "0,/$pattern/p" | tac
}
export -f _get_last_text

_latest_logs() {
    [ "$DEBUG" = 0 ] && return
    _tit "latest node logs"
    for node_num in 1 2 3; do
        _subtit "node $node_num"
        $TMUX_CMD capture-pane -ep -t "node$node_num" -S -20 | grep -v '^$'
    done
}

_wait_for_text() {
    local timeout pane pattern lines
    timeout="$1"
    pane="$2"
    pattern="$3"
    lines="${4:-0}"
    timeout  --foreground "$timeout" bash <<EOT
    while :; do
        _get_last_text "$pane" "$pattern" | grep -A$lines "$pattern" && break
        sleep 1
    done
EOT
    [ $? = 0 ] || _die "expected output not found, exiting..."
}

_wait_for_text_multi() {
    local timeout pane pattern1 pattern2 lines
    timeout="$1"
    pane="$2"
    pattern1="$3"
    pattern2="$4"
    lines="${5:-0}"
    timeout  --foreground "$timeout" bash <<EOT
    while :; do
        _get_last_text "$pane" "$pattern1" | grep -A$lines "$pattern2" && break
        sleep 1
    done
EOT
    [ $? = 0 ] || _die "expected output not found, exiting..."
}


timestamp() {
    [ "$DEBUG" != 0 ] && date +%T
}

check() {
    local num="$1"
    _subtit "checking output from node $num"
}

get_address() {
    local num="$1"
    _subtit "getting an address from node $num"
    $TMUX_CMD send-keys -t node$num "getaddress" C-m
    address=$(_wait_for_text 5 node$num "Address:" \
        | head -1 | grep -Eo '[0-9a-z]{40,48}')
    _out "address: $address"
}

fund_address() {
    local address txid
    address="$1"
    _subtit "funding address $address"
    txid=$($BITCOIN_CLI sendtoaddress "$address" 1)
    _out "txid: $txid"
    mine 1
}

sync_wallet() {
    local num="$1"
    $TMUX_CMD send-keys -t node$num "syncwallet" C-m
}

create_utxos() {
    local num get_funds
    num="$1"
    get_funds="${2:-1}"
    _tit "creating UTXOs on node $num"
    if [ "$get_funds" != 0 ]; then
        get_address $num
        fund_address $address
    fi
    _subtit "calling createutxos"
    $TMUX_CMD send-keys -t node$num "createutxos" C-m
    timestamp
    _wait_for_text_multi $T_1 node$num "createutxos" "UTXO creation complete"
    timestamp
    mine 1
}

get_node_ids() {
    _tit "get node IDs"
    node1_id=$(_wait_for_text 1 node1 "Local Node ID is" |awk '{print $NF}')
    node2_id=$(_wait_for_text 1 node2 "Local Node ID is" |awk '{print $NF}')
    node3_id=$(_wait_for_text 1 node3 "Local Node ID is" |awk '{print $NF}')
    _out "node 1 ID: $node1_id"
    _out "node 2 ID: $node2_id"
    _out "node 3 ID: $node3_id"
}

mine() {
    local blocks="$1"
    _subtit "mining $blocks block(s)"
    $TMUX_CMD send-keys -t node1 "mine $blocks" C-m
    sleep 3
}

issue_asset() {
    local rgb_amt=1000
    _tit "issue RGB asset ($rgb_amt)"
    $TMUX_CMD send-keys -t node1 "issueasset $rgb_amt USDT Tether 0" C-m
    asset_id=$(_wait_for_text 5 node1 "Asset ID:" |awk '{print $NF}')
    _out "asset ID: $asset_id"
    sleep 1
}

asset_balance() {
    local num="$1"
    _subtit "asset balance on node $num"
    $TMUX_CMD send-keys -t node$num "assetbalance $asset_id" C-m
    _wait_for_text_multi $T_1 node$num "assetbalance" "Asset balance"
    _wait_for_text $T_1 node$num ">" >/dev/null
}

blind() {
    local num="$1"
    _tit "generate blinded UTXO on node $num"
    $TMUX_CMD send-keys -t node$num "receiveasset" C-m
    blinded_utxo="$(_wait_for_text $T_1 node$num 'Blinded UTXO:' \
        | grep -Eo '[0-9a-z]+$')"
    _out "blinded UTXO: $blinded_utxo"
}

send_assets() {
    local num rgb_amt
    num="$1"
    rgb_amt="$2"
    _tit "send $rgb_amt RGB assets from node $num to blinded UTXO $blinded_utxo"
    $TMUX_CMD send-keys -t node$num "sendasset $asset_id $rgb_amt $blinded_utxo" C-m
    timestamp
    check $num
    _wait_for_text_multi $T_1 node$num "sendasset" "RGB send complete"
    timestamp
    sleep 1
}

refresh() {
    local num="$1"
    _tit "refresh on node $num"
    $TMUX_CMD send-keys -t node$num "refresh" C-m
    timestamp
    check $num
    _wait_for_text $T_5 node$num "Refresh complete"
    timestamp
    sleep 1
}

open_colored_channel() {
    local src_num dst_num dst_port dst_id rgb_amt
    src_num="$1"
    dst_num="$2"
    dst_port="$3"
    dst_id="$4"
    rgb_amt="$5"
    _tit "open channel from node $src_num to node $dst_num with $rgb_amt assets"
    $TMUX_CMD send-keys -t node$src_num "opencoloredchannel $dst_id@127.0.0.1:$dst_port 999666 546000 $asset_id $rgb_amt --public" C-m
    check $src_num
    _wait_for_text_multi $T_5 node$src_num "opencoloredchannel" "HANDLED ACCEPT CHANNEL"
    timestamp
    _wait_for_text_multi $T_5 node$src_num "opencoloredchannel" "FUNDING COMPLETED"
    timestamp
    _wait_for_text_multi $T_5 node$src_num "opencoloredchannel" "HANDLED FUNDING SIGNED"
    timestamp
    check $dst_num
    _wait_for_text $T_5 node$dst_num "HANDLED OPEN CHANNEL"
    timestamp
    _wait_for_text_multi $T_5 node$dst_num "HANDLED OPEN CHANNEL" "HANDLED FUNDING CREATED"
    timestamp

    mine 6
    check $src_num
    _wait_for_text_multi $T_10 node$src_num "EVENT: Channel .* with peer .* is ready to be used"
    timestamp
    _wait_for_text_multi $T_5 node$src_num "EVENT: Channel .* with peer .* is ready to be used" "HANDLED CHANNEL UPDATE"
    timestamp
    check $dst_num
    _wait_for_text $T_5 node$dst_num "EVENT: Channel .* with peer .* is ready to be used"
    timestamp
    _wait_for_text_multi $T_5 node$dst_num "EVENT: Channel .* with peer .* is ready to be used" "HANDLED CHANNEL UPDATE"
    timestamp
    sleep 3

    $TMUX_CMD send-keys -t node$src_num "listchannels" C-m
    sleep 1
    channel_id=$(_wait_for_text 5 node$src_num "[^_]channel_id:" \
        | head -1 | grep -Eo '[0-9a-f]{64}')
    _out "channel ID: $channel_id"
}

open_vanilla_channel() {
    local src_num dst_num dst_port dst_id msat_amount
    src_num="$1"
    dst_num="$2"
    dst_port="$3"
    dst_id="$4"
    msat_amount="$5"
    _tit "open channel from node $src_num to node $dst_num of $msat_amount mSAT"
    $TMUX_CMD send-keys -t node$src_num "openchannel $dst_id@127.0.0.1:$dst_port $msat_amount 546000 --public" C-m
    check $src_num
    _wait_for_text_multi $T_5 node$src_num "openchannel" "HANDLED ACCEPT CHANNEL"
    timestamp
    _wait_for_text_multi $T_5 node$src_num "openchannel" "FUNDING COMPLETED"
    timestamp
    _wait_for_text_multi $T_5 node$src_num "openchannel" "HANDLED FUNDING SIGNED"
    timestamp
    check $dst_num
    _wait_for_text $T_5 node$dst_num "HANDLED OPEN CHANNEL"
    timestamp
    _wait_for_text_multi $T_5 node$dst_num "HANDLED OPEN CHANNEL" "HANDLED FUNDING CREATED"
    timestamp

    mine 6
    # check $src_num
    # _wait_for_text_multi $T_10 node$src_num "EVENT: Channel .* with peer .* is ready to be used"
    # timestamp
    # _wait_for_text_multi $T_5 node$src_num "EVENT: Channel .* with peer .* is ready to be used" "HANDLED CHANNEL UPDATE"
    # timestamp
    # check $dst_num
    # _wait_for_text $T_5 node$dst_num "EVENT: Channel .* with peer .* is ready to be used"
    # timestamp
    # _wait_for_text_multi $T_5 node$dst_num "EVENT: Channel .* with peer .* is ready to be used" "HANDLED CHANNEL UPDATE"
    # timestamp
    sleep 3

    $TMUX_CMD send-keys -t node$src_num "listchannels" C-m
    sleep 1
    channel_id=$(_wait_for_text 5 node$src_num "[^_]channel_id:" \
        | head -1 | grep -Eo '[0-9a-f]{64}')
    _out "channel ID: $channel_id"
}


list_channels() {
    local node_num chan_num lines text matches
    node_num="$1"
    chan_num="${2:-1}"
    lines=$((chan_num*20))
    _subtit "list channels ($chan_num expected) on node $node_num"
    $TMUX_CMD send-keys -t node$node_num "listchannels" C-m
    sleep 1
    text="$(_wait_for_text 5 node$node_num "listchannels" $lines | sed -n '/^\[/,/^\]/p')"
    echo "$text"
    matches=$(echo "$text" | grep -c "is_channel_ready: true")
    [ "$matches" = "$chan_num" ] || _die "one or more channels not ready"
}

list_payments() {
    local num="$1"
    _tit "list payments on node $num"
    $TMUX_CMD send-keys -t node$num "listpayments" C-m
    text="$(_wait_for_text 5 node$num "listpayments" 10 | sed -n '/^\[/,/^\]/p')"
    echo "$text"
    _wait_for_text $T_1 node$num ">" >/dev/null
}

close_channel() {
    local src_num dst_num dst_id
    src_num="$1"
    dst_num="$2"
    dst_id="$3"
    _tit "close channel from node $src_num (cooperative)"
    $TMUX_CMD send-keys -t node$src_num "closechannel $channel_id $dst_id" C-m
    timestamp
    check $src_num
    _wait_for_text_multi $T_5 node$src_num "closechannel" "HANDLED SHUTDOWN"
    timestamp
    _wait_for_text_multi $T_5 node$src_num "closechannel" "GENERATED CLOSING SIGNED"
    timestamp
    check $dst_num
    _wait_for_text_multi $T_5 node$dst_num "HANDLED SHUTDOWN" "EVENT: Channel .* closed due to: CooperativeClosure"
    timestamp

    mine 6
    check $dst_num
    _wait_for_text_multi $T_5 node$dst_num "EVENT: Channel .* closed" "Event::SpendableOutputs complete"
    timestamp

    check $src_num
    _wait_for_text_multi $T_5 node$src_num "EVENT: Channel .* closed" "Event::SpendableOutputs complete"
    timestamp
    mine 1
}

forceclose_channel_init() {
    local src_num dst_id
    src_num="$1"
    dst_id="$2"
    _tit "close channel from node $src_num (unilateral)"
    $TMUX_CMD send-keys -t node$src_num "forceclosechannel $channel_id $dst_id" C-m
    timestamp
    check $src_num
    _wait_for_text $T_5 node$src_num "EVENT: Channel .* closed due to: HolderForceClosed"
    timestamp
}

forceclose_channel() {
    local src_num dst_num dst_id
    src_num="$1"
    dst_num="$2"
    dst_id="$3"

    forceclose_channel_init $src_num $dst_id

    check $dst_num
    _wait_for_text $T_1 node$dst_num "EVENT: Channel .* closed due to: CounterpartyForceClosed"
    timestamp

    mine 6
    check $dst_num
    _wait_for_text $T_5 node$dst_num "Event::SpendableOutputs complete"
    timestamp

    mine 144
    check $src_num
    _wait_for_text $T_5 node$src_num "Event::SpendableOutputs complete"
    timestamp
    mine 1
}

keysend_init() {
    local src_num dst_num dst_id rgb_amt
    src_num="$1"
    dst_num="$2"
    dst_id="$3"
    rgb_amt="$4"

    _tit "send $rgb_amt assets from node $src_num to node $dst_num"
    $TMUX_CMD send-keys -t node$src_num "keysend $dst_id 3000000 $asset_id $rgb_amt" C-m
    timestamp
    check $src_num
    _wait_for_text_multi $T_5 node$src_num "keysend" "EVENT: initiated sending"
    timestamp
}

keysend() {
    local src_num dst_num dst_id rgb_amt
    src_num="$1"
    dst_num="$2"
    dst_id="$3"
    rgb_amt="$4"

    keysend_init $src_num $dst_num $dst_id $rgb_amt

    _wait_for_text_multi $T_15 node$src_num "keysend" "EVENT: successfully sent payment"
    timestamp
    _wait_for_text_multi $T_5 node$src_num "EVENT: successfully sent payment" "HANDLED REVOKE AND ACK"
    timestamp

    check $dst_num
    _wait_for_text $T_5 node$dst_num "EVENT: received payment"
    timestamp
    _wait_for_text $T_5 node$dst_num "Event::PaymentClaimed end"
    timestamp
    _wait_for_text_multi $T_5 node$dst_num "Event::PaymentClaimed end" "HANDLED COMMITMENT SIGNED"
    timestamp
}

get_colored_invoice() {
    local num rgb_amt text pattern
    num="$1"
    rgb_amt="$2"

    _tit "get invoice for $rgb_amt assets from node $num"
    $TMUX_CMD send-keys -t node$num "getcoloredinvoice 3000000 900 $asset_id $rgb_amt" C-m
    timestamp
    check $num
    pattern="SUCCESS: generated invoice: "
    text="$(_wait_for_text_multi $T_5 node$num \
        'getcoloredinvoice' "$pattern" 3 \
        | sed "s/$pattern//" |grep -Eo '^[0-9a-z]+$')"
    timestamp
    invoice="$(echo $text | sed -E 's/[\n ]//g')"
    _out "invoice: $invoice"
}

get_vanilla_invoice() {
    local num msat_amount text pattern
    num="$1"
    msat_amount="$2"

    _tit "get invoice for $msat_amount mSATs from node $num"
    $TMUX_CMD send-keys -t node$num "getinvoice $msat_amount 900" C-m
    timestamp
    check $num
    pattern="SUCCESS: generated invoice: "
    text="$(_wait_for_text_multi $T_5 node$num \
        'getinvoice' "$pattern" 3 \
        | sed "s/$pattern//" |grep -Eo '^[0-9a-z]+$')"
    timestamp
    invoice="$(echo $text | sed -E 's/[\n ]//g')"
    _out "invoice: $invoice"
}

send_payment() {
    local src_num dst_num invoice
    src_num="$1"
    dst_num="$2"
    invoice="$3"

    _tit "pay invoice from node $src_num"
    $TMUX_CMD send-keys -t node$src_num "sendpayment $invoice" C-m
    timestamp
    check $src_num
    _wait_for_text_multi $T_5 node$src_num "sendpayment" "EVENT: initiated sending"
    timestamp
    _wait_for_text_multi $T_15 node$src_num "sendpayment" "EVENT: successfully sent payment"
    timestamp

    check $dst_num
    _wait_for_text $T_5 node$dst_num "EVENT: received payment"
    timestamp
    _wait_for_text $T_5 node$dst_num "Event::PaymentClaimed end"
    timestamp
    _wait_for_text_multi $T_5 node$dst_num "Event::PaymentClaimed end" "HANDLED COMMITMENT SIGNED"
    timestamp
}
