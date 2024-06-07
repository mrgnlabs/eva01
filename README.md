
# Eva01

Marginfi liquidator

## Deployment Guide
### Installing dependencies

Ubuntu 
```bash
  sudo apt install build-essential libssl-dev pkg-config
```

### Creating a New Configuration File

To initiate the creation of a new configuration file for the liquidator, execute the following command in your terminal:

```bash
  eva01 setup
```

This command launches a Mini CLI wizard that guides you through setting up a base configuration for the liquidator. During this process, it will also check if you have a MarginfiAccount initialized. If not, it will prompt you to create one. At this stage, the setup will only request the essential variables. For adjusting settings like `Minimum Profit`, you'll need to manually edit the configuration file afterward.
Now, lets start the liquidator

### Starting the liquidator

Once your configuration file is ready, you can start the liquidator by running:

```bash
  eva01 run <config.toml>
```

Replace `<config.toml>` with the path to your newly created configuration file. After initiating this command, Eva begins its operation. Please note that it might take a few minutes for Eva to load all the marginfi accounts, including English support, and to be fully operational.

### Initial Loading Time

The initial loading phase can take some time, depending on your RPC. Eva will load everything needed into the state, including all Marginfi Accounts. Expect the loading time to be between 1-3 minutes depending on the RPC.
## Eva01 Configuration

To run eva you need to add configuration variables first.

## Required Configuration

The following are mandatory to run Eva

- `RPC_URL` The RPC endpoint URL as a string.
- `YELLOWSTONE_ENDPOINT` The Yellowstone endpoint URL as a string.
- `KEYPAIR_PATH` The wallet keypair for the liquidator. It is a string that is the path of the file containing the Keypair object.
- `SIGNER_PUBKEY` The pubkey corresponding to the keypair
- `LIQUIDATOR_ACCOUNT` The marginfi account corresponding to the `SIGNER_PUBKEY`



