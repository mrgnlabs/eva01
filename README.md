
# Eva01 - the Marginfi Liquidator

## Structure
* `bin` - shell scripts and environment configuration file templare
* `src` - source code
* eva.Dockerfile - the Docker configuration for building an image to run on Kubernetes.

### Configuration
The [env.template](bin/env.template) file is a template for required and optional environment variables that are used by Eva.

## Deployment
### Linux box
1. Install dependencies
    * OS librarires: `sudo apt install build-essential libssl-dev pkg-config unzip`
    * Protoc:  https://grpc.io/docs/protoc-installation/#install-pre-compiled-binaries-any-os;
    * Rust: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
1. Clone the Git repo.
1. Create and configure the `.env`  file by using [env.template](bin/env.template) as prototype.

 > VSCode: create the VSCode launch configuration and add the configured `.env` to it.

### Kubernetes
1. Run [Build and Upload App Image](https://github.com/mrgnlabs/eva01/actions/workflows/build-app-image.yml) GitHub Action to build new Eva Docker image and upload it to the [Infra artifacts registry](https://console.cloud.google.com/artifacts/docker/mrgn-shared/us-central1/shared-artifact-registry/eva/).
1. Run the [Deploy Full Platform](https://github.com/mrgnlabs/mrgn-core/actions/workflows/deploy.yml) `mrgn-core` GitHub Action workflow to deploy new Eva Docker image to Kubernetes cluster. 
   - Use the `eva` deployment group.

## Run
### Linux box
1. Source the `.env` file. Example: `source src/eva01/bin/prod.env`
1. Optionally Rotate logs: `mv  ~/log/liquidator.log  ~/log/liquidator.log.$(date +'%Y%m%dT%H%M%S')`
1. Run the Liquidator: `nohup bash $LIQUIDATOR_SRC_PATH/bin/start.sh >> ~/log/liquidator.log 2>&1 &`

> Initial Loading Time
The initial loading phase can take some time, depending on your RPC. Eva will load everything needed into the state, including all Marginfi Accounts. Expect the loading time to be between 1-3 minutes depending on the RPC.

> Local Docker: Run `docker build -f <CONFIG FILE> -t eva:latest .` to build an image and `docker run --env-file docker.staging.env --rm -v <WALLET>:<WALLET> eva` to run it.

### Kubernetes
1. Build and deploy the new Eva docker image.
1. Startup the `eva` workload on the [mrgn-stage](https://console.cloud.google.com/kubernetes/workload/overview?inv=1&invt=Ab0eUg&project=mrgn-stage&supportedpurview=project) or [mrgn-prod](https://console.cloud.google.com/kubernetes/workload/overview?inv=1&invt=Ab0eUg&project=mrgn-prod&supportedpurview=project) cluster.