# Liquidator

Multi-chain DeFi liquidation bot written in Rust. Monitors borrower positions across Aave V3, Compound V3, and Venus, executing profitable liquidations with MEV protection.

## Features

- **Adaptive scanning** - Tiered monitoring reduces RPC usage by 80-90% during stable markets
- **Multi-chain** - Supports Base, Polygon, Arbitrum, Avalanche, BNB Chain
- **Multi-protocol** - Aave V3, Compound V3, Venus
- **RPC failover** - Automatic rotation between free and premium endpoints
- **MEV protection** - Flashbots integration for private transaction submission

## How It Works

The bot uses an event-driven architecture with three scanning tiers:

| Tier | Interval | Target |
|------|----------|--------|
| Critical | 2s | Positions with HF < 1.1 |
| Watchlist | 10s | Positions with HF < 1.5 |
| Full | 2min | All tracked borrowers |

Positions are automatically promoted/demoted between tiers based on health factor changes. Price volatility (>5% moves) triggers immediate rescans.

## Requirements

- Rust 1.70+
- RPC endpoints (WebSocket + HTTP) for each chain
- Private key with gas funds on target chains

## Installation
```bash
git clone https://github.com/in8forge/liquidator.git
cd liquidator
cargo build --release
```

## Configuration

Create a `.env` file:
```bash
# Required
PRIVATE_KEY=0x...
DRY_RUN=true

# RPC endpoints (comma-separated for failover)
BASE_RPC_URLS=wss://base-mainnet.g.alchemy.com/v2/...,https://mainnet.base.org
POLYGON_RPC_URLS=wss://polygon-mainnet.g.alchemy.com/v2/...
ARBITRUM_RPC_URLS=wss://arb-mainnet.g.alchemy.com/v2/...
AVALANCHE_RPC_URLS=wss://api.avax.network/ext/bc/C/ws
BNB_RPC_URLS=wss://bsc-mainnet.nodereal.io/ws/v1/...

# Protocol addresses (per chain)
BASE_POOL_ADDRESS=0x...
BASE_DATA_PROVIDER=0x...

# Optional
MIN_PROFIT_USD=10
MEV_THRESHOLD_USD=50
DISCORD_WEBHOOK=https://discord.com/api/webhooks/...
HEALTH_PORT=8080
```

## Usage
```bash
# Dry run (recommended first)
DRY_RUN=true cargo run --release

# Live
DRY_RUN=false cargo run --release
```

The bot exposes a health endpoint at `http://localhost:8080/health` for monitoring.

## Architecture
```
src/
├── main.rs          # Entry point, orchestration
├── scanner.rs       # Adaptive scanning, liquidation detection
├── chains.rs        # Multi-RPC management, failover
├── protocols/
│   ├── aave.rs      # Aave V3 integration
│   ├── compound.rs  # Compound V3 (Comet) integration
│   └── venus.rs     # Venus integration
├── executor.rs      # Transaction execution, MEV
├── oracle.rs        # Price feeds, WebSocket subscriptions
└── borrowers.rs     # Position tracking, persistence
```

## Monitoring

Discord notifications for:
- Startup/shutdown
- Liquidation attempts (success/fail)
- Significant price movements
- Auto-withdrawals from liquidator contracts

Stats logged every 60 seconds:
```
Events: 142 | Checks: 89 | Attempted: 3 | Success: 2 | Failed: 1 | Skipped: 12 | Competitor: 1
```

## Disclaimer

This software is provided as-is. Running liquidation bots involves financial risk. You are responsible for:

- Securing your private keys
- Understanding gas costs and potential losses
- Complying with applicable laws
- Testing thoroughly in dry-run mode first

## License

MIT
