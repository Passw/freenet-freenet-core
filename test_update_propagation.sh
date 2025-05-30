#!/bin/bash
# Test script to run the partially connected network test with enhanced logging

cd /home/ian/code/freenet/freenet-core/debug-update-issues

echo "Running partially connected network test with enhanced update/subscribe logging..."
echo "This test uses 1 gateway + 7 nodes with 30% connectivity"
echo ""

# Run the test with debug logging directed to a file
RUST_LOG=freenet=debug cargo test -p freenet-ping-app test_ping_partially_connected_network -- --nocapture 2>&1 | tee update_propagation_test.log

echo ""
echo "Test complete. Log saved to update_propagation_test.log"
echo ""
echo "Key things to look for in the log:"
echo "1. 'Routing decision' - shows how peers are selected for routing"
echo "2. 'Broadcasting update' - shows which peers receive update broadcasts"
echo "3. 'Contract now has X subscribers' - tracks subscription state"
echo "4. 'Received BroadcastTo message' - confirms update arrival at peers"
echo ""
echo "To see routing decisions:"
echo "grep 'Routing decision' update_propagation_test.log"
echo ""
echo "To see broadcast targets:"
echo "grep 'Broadcasting update' update_propagation_test.log"
echo ""
echo "To see subscription tracking:"
echo "grep 'subscribers' update_propagation_test.log"