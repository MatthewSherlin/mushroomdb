### Added
- **`mushroomdb serve --tx-max <n>`, `--tx-timeout-secs <secs>` and `--tx-max-lifetime-secs <secs>`** set the HTTP transaction limits; each flag wins over `MUSHROOMDB_TX_MAX`, `MUSHROOMDB_TX_TIMEOUT_SECS` and `MUSHROOMDB_TX_MAX_LIFETIME_SECS`, and must be a positive integer. Bolt keeps `--bolt-tx-timeout-secs`.
