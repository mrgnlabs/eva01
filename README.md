# Eva 01

## Architecture

### Components
All components run concurrently, the main idea is that the liquidator is not blocked by liquidating a single account at a time, but will liquidate and manage risk real time.

1. State Engine
    1. Yellowstone geyser receives updates for:
        - All marginfi accounts 
        - Oracle accounts
        - Bot token accounts
2. Account Liquidator
    1. Observe marginfi accounts and select unhealty ones
    2. Send liquidation txs for unhealthy accounts
    3. Stop and wait if liquidator health falls under safety threshold
3. Seller
    1. Swap collateral into liabs with flashloans and jupiter api
    2. Sell of remanining collateral


### Lifecycle

1. Startup
    1. Subscribe to state updates for marginfi accounts, oracles and token accounts.
    2. Load all marginfi accounts, oracles and token accounts via RPC
2. Listening
    1. Update state in the state engine
    2. On tick or oracle update check health of all accounts, and liquidate if unhealthy and risk acceptable
    3. Sell collateral and repay debt
3. Reloading

## Helper tools
### Smart contracts
- Observable accounts builder: Automatically build the observable accounts array based on the active marginfi account lending account balances.
- Health circuit breaker: Tx that fails if account health falls under a threshold