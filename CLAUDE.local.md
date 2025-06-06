- contract states are commutative monoids, they can be "merged" in any order to arrive at the same result. This may reduce some potential race conditions.

## Testing Ping App

When running tests for the freenet-ping app, you must:

1. Set the CARGO_TARGET_DIR environment variable to the project root's target directory:
   ```bash
   export CARGO_TARGET_DIR=/home/ian/code/freenet/freenet-core/main/target
   ```

2. Build the ping contract WASM file before running tests:
   ```bash
   cd apps/freenet-ping/contracts/ping
   cargo build --target wasm32-unknown-unknown
   ```

3. The tests expect to find the compiled WASM at: `$CARGO_TARGET_DIR/wasm32-unknown-unknown/debug/freenet_ping_contract.wasm`

Note: The ping app tests use a hardcoded path that points to `../target/wasm32-unknown-unknown/debug/freenet_ping_contract.wasm` relative to the app directory. This is why CARGO_TARGET_DIR must be set correctly.