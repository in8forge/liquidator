# 🦀 Liquidator V8.0 - Rust Edition

High-performance multi-chain DeFi liquidation bot written in Rust.

## Why Rust?

| Metric | Node.js V7.5 | Rust V8.0 |
|--------|--------------|-----------|
| Latency | ~100-500ms | ~10-50ms |
| Memory | ~150MB | ~20MB |
| Concurrency | Event loop | True parallelism |
| Type Safety | Runtime errors | Compile-time |

## Features

- 🔥 **Multi-chain**: Base, Polygon, Arbitrum, Avalanche, BNB
- 🔥 **Multi-protocol**: Aave V3, Compound V3, Venus
- ⚡ **Fast**: Native async with Tokio
- 🛡️ **MEV Protection**: Flashbots integration
- 📊 **Health endpoint**: `/health` for monitoring
- 🔌 **Circuit breaker**: Auto-pause on failures
- 🧪 **Dry run mode**: Test without executing

## Quick Start

### Prerequisites

- Rust 1.75+ (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- Funded wallet with gas on target chains

### Build

```bash
# Debug build
cargo build

# Release build (optimized)
cargo build --release
```

### Configure

```bash
cp .env.example .env
# Edit .env with your keys
```

### Run

```bash
# Debug mode
cargo run

# Release mode
cargo run --release

# With logging
RUST_LOG=info cargo run --release
```

## Project Structure

```
liquidator-rs/
├── Cargo.toml          # Dependencies
├── src/
│   ├── main.rs         # Entry point
│   ├── config.rs       # Configuration
│   ├── types.rs        # Data structures
│   ├── chains.rs       # Multi-chain management
│   ├── executor.rs     # TX execution
│   ├── health.rs       # Health server
│   ├── discord.rs      # Notifications
│   └── protocols/
│       ├── mod.rs
│       ├── aave.rs     # Aave V3
│       ├── compound.rs # Compound V3
│       └── venus.rs    # Venus
└── .env.example
```

## Performance Optimizations

1. **Async runtime**: Tokio with work-stealing scheduler
2. **Zero-copy**: Uses `alloy` for efficient EVM interactions
3. **Lock-free**: DashMap for concurrent price caching
4. **Batch RPC**: Multicall3 for position checking
5. **Release profile**: LTO, single codegen unit, stripped binary

## Build Profiles

```toml
[profile.release]
opt-level = 3      # Max optimization
lto = true         # Link-time optimization
codegen-units = 1  # Better optimization
panic = "abort"    # Smaller binary
strip = true       # Remove debug symbols
```

## Health Endpoint

```bash
curl http://localhost:3847/health
```

```json
{
  "status": "healthy",
  "chains": ["base", "polygon", "arbitrum"],
  "stats": {
    "events": 1523,
    "checks": 120,
    "liquidations": 2
  },
  "circuit_breaker": {
    "is_open": false,
    "consecutive_failures": 0
  }
}
```

## Roadmap

- [x] Project scaffold
- [x] Config management
- [x] Chain connections
- [x] Health endpoint
- [x] Aave position checking (Multicall batched)
- [x] Circuit breaker
- [x] Execution locks with timeout
- [x] Nonce management
- [x] Discord notifications
- [x] Graceful shutdown
- [x] WebSocket price feeds (Chainlink)
- [x] Borrower discovery
- [x] Borrower persistence (JSON)
- [x] Scanner module
- [x] Profit simulation
- [x] Competitor detection
- [x] Dry run mode
- [ ] Compound V3 full support
- [ ] Venus full support
- [ ] Flash loan TX building
- [ ] MEV protection (Flashbots)
- [ ] Swap path validation
- [ ] Profit withdrawal

## License

MIT

## Author

Doss - in8forge@proton.me
