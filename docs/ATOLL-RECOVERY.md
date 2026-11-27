# Copy registry backups to Nextcloud with Atoll Recovery

Atoll Recovery on StartOS can copy the nightly encrypted FedimintCard Ops registry snapshot to Nextcloud and read it back to verify the bytes. It can monitor this registry alongside CashuCard issuers. Each registry backup includes all operators’ card records and card-manager escrow. Only the registry deployment owner should authorize a reader. Keep the age recovery identity offline.

## Connect a registry

1. Assign a stable UUID to this registry deployment. Use a different UUID for staging and production. On the deployment owner’s machine, run the following from the `registry/` directory, replacing `REGISTRY_UUID` with that UUID. The command prints a private ten-minute pairing code to the terminal and passes only the hashed grant to Wrangler. For staging, append `--env staging` to the Wrangler command.

   ```sh
   node scripts/atoll-grant.mjs REGISTRY_UUID | npx wrangler secret put ATOLL_RECOVERY_GRANT
   ```

2. Stop Atoll Recovery. If it already backs up another issuer, choose **Connect Additional Issuer** (beta.7 or later). Give FedimintCard Ops its own name and Nextcloud directory. If this is your first issuer, use **Connect Issuer** and **Connect Nextcloud**. Enter the registry’s HTTPS origin and the private pairing code. Leave **verified-copy receipts** off. Atoll verifies the copy and records the result locally.
3. Start Atoll Recovery. After the registry’s first successful nightly backup, check **Recovery Status** and the encrypted archive and matching JSON receipt in Nextcloud.

One grant pairs one reader. Retrying the same claim with the same device is safe. To rotate it, create a new grant using the same registry UUID and re-pair the same Atoll profile. Replacing or removing the grant revokes the prior reader. Expiry limits first pairing. An enrolled reader keeps working until the grant changes. The reader can only list and download encrypted exports. Operators and tills cannot authorize a reader.

## Check a restore offline

Download an archive and its matching `.json` receipt from the Atoll directory in Nextcloud. On a trusted recovery machine with Node 22.18 or later and the registry’s age identity, run:

```sh
cd registry
npm ci
node scripts/restore-nextcloud-copy.mjs /path/to/archive.age /path/to/age-identity.txt /path/to/archive.age.json
```

The drill verifies the ciphertext against the receipt, decrypts it locally and imports every exported table into a new in-memory registry database. It checks row counts and SQLite integrity, prints counts, and discards the database. Use the registry revision matching the snapshot schema. Operator passwords and till credentials are excluded from snapshots. An operational recovery requires new operator credentials and replacement till tokens. Switching a live deployment to a restored database is a separate operation.
