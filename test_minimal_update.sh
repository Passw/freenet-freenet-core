#!/bin/bash
# Minimal test script to debug update propagation with just 2 gateways + 2 nodes

cd /home/ian/code/freenet/freenet-core/debug-update-issues

echo "🧪 Running minimal update propagation test (2 gateways + 2 nodes)..."
echo "This test isolates the update propagation issue with full connectivity"
echo ""

# Run the test with debug logging
RUST_LOG=freenet=debug cargo test -p freenet-ping-app test_minimal_update_propagation -- --nocapture 2>&1 | tee minimal_update_test.log

echo ""
echo "Test complete. Log saved to minimal_update_test.log"
echo ""
echo "Quick analysis commands:"
echo "========================================"
echo "See routing decisions:"
echo "  grep 'Routing decision' minimal_update_test.log | head -20"
echo ""
echo "See subscription flow:"
echo "  grep -E '(Starting subscribe|add subscriber|subscribers)' minimal_update_test.log"
echo ""
echo "See update broadcast flow:"
echo "  grep -E '(Broadcasting update|Broadcast target|BroadcastTo message)' minimal_update_test.log"
echo ""
echo "See critical warnings:"
echo "  grep -E '(WARN|No subscribers found)' minimal_update_test.log"
echo ""
echo "Full trace of update from Node 0:"
echo "  grep -A5 -B5 'test-update-from-node-0' minimal_update_test.log"