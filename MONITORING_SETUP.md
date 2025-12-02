# Local Prometheus & Grafana Setup

This guide helps you visualize Eva01 metrics locally using Prometheus and Grafana.

## Quick Start

1. **Start the monitoring stack:**
   ```bash
   docker-compose -f docker-compose.monitoring.yml up -d
   ```

2. **Access the services:**
   - **Prometheus UI**: http://localhost:9090
   - **Grafana UI**: http://localhost:3001
     - Username: `admin`
     - Password: `admin` (change on first login)

3. **Configure Grafana:**
   - Go to Configuration → Data Sources → Add data source
   - Select "Prometheus"
   - URL: `http://prometheus:9090`
   - Click "Save & Test"

4. **Import the pre-built dashboard:**
   - Go to Dashboards → Import
   - Click "Upload JSON file"
   - Select `grafana-dashboard.json` from this directory
   - Select your Prometheus data source
   - Click "Import"

   The dashboard includes:
   - **Overview Stats**: Total attempts, successes, failures, liquidatable accounts
   - **Liquidation Metrics**: Attempt rates, success vs failure rates, failure reasons breakdown
   - **Account Scanning**: Scan rates, liquidatable accounts over time, scan duration percentiles
   - **Performance**: Liquidation latency percentiles (P50, P95, P99)
   - **Geyser Metrics**: Update rates and triggered scan rates
   - **Error Tracking**: Error rates with thresholds
   - **Status Indicators**: Scan in progress flag, total accounts scanned

## Stop the monitoring stack

```bash
docker-compose -f docker-compose.monitoring.yml down
```

## Troubleshooting

If Prometheus can't reach your metrics endpoint:
- Make sure your Eva01 app is running and metrics are available at `http://localhost:9000/metrics`
- On Linux, you may need to change `host.docker.internal` to your host IP or use `network_mode: host` in docker-compose

## Alternative: Quick Query with Prometheus UI

You can also just use the Prometheus UI directly:
1. Go to http://localhost:9090
2. Use the "Graph" tab to query metrics
3. Try queries like:
   - `eva01_eva01_liquidation_attempts_total`
   - `rate(eva01_eva01_geyser_updates_total[5m])`

