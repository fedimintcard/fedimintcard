# Run a Card 4.0 merchant till

This image accepts **Fedimint Card 4.x cards only** and requires a **Card 4.0 registry**. Check your cards and registry version before upgrading a till that serves older cards. Use the Card 4.0 generation of the POS app for these cards.

The till uses `https://registry.fedimintcard.org` when `OPS_URL` is unset. To use another Card 4.0 registry, add a line such as `OPS_URL=https://registry.example.org` to the `.env` file beside `compose.yaml`. Use your registry’s HTTPS origin. Run `docker compose up -d` to apply the setting before connecting the till to the registry.

For a build published by source commit, set `TILL_IMAGE=ghcr.io/fedimintcard/till:sha-FULL_COMMIT_SHA` in your Compose environment. For an exact copy, use the published multi-architecture index digest (`ghcr.io/fedimintcard/till@sha256:INDEX_DIGEST`).

Use the Compose file supplied with the till release. The image supports Linux amd64 and arm64. Choose a private admin password of at least 16 visible ASCII characters in your Compose environment. Keep the password in your password manager. The image name is `ghcr.io/fedimintcard/till`. Enter these commands in Bash.

```sh
read -rs -p 'Till admin password: ' TILL_ADMIN_PASSWORD && echo
export TILL_ADMIN_PASSWORD
docker compose up -d
```

On the computer running Docker, open `http://127.0.0.1:9878`. For setup from a phone, put this port behind an HTTPS reverse proxy or a private encrypted connection. The admin page holds the payout controls.

The phone API uses port 9877. Give it a separate HTTPS address and set `TILL_POS_PUBLIC_URL` to the complete origin, for example `https://pos.example.com`. The admin page uses that address when it creates the phone pairing QR code. Forward WebSocket upgrades and allow responses to take over two minutes. Both ports bind to loopback by default. An operator can set `TILL_POS_BIND` and `TILL_ADMIN_BIND` independently to choose a different bind address. In a deployment with a secret file, mount it read-only, set `TILL_ADMIN_PASSWORD_FILE` to its container path and remove the password environment variable. UID 1000 must be able to read the file.

For phones on the same trusted LAN as the Docker host, Compose can derive the pairing address from the published IP and port. On a Linux host with iproute2 or a Mac using Docker Desktop, use the following commands in the Compose shell. They select the IPv4 address of the outbound interface. Check that this is the network your phones use, especially on machines with a VPN. Use a fixed lease for a till that stays on this network.

```sh
case "$(uname -s)" in
  Linux) TILL_POS_BIND=$(ip -4 route get 192.0.2.1 | awk '{for(i=1;i<NF;i++) if($i=="src") {print $(i+1); exit}}') ;;
  Darwin) TILL_POS_BIND=$(ipconfig getifaddr "$(route -n get default | awk '$1=="interface:" {print $2; exit}')") ;;
esac
export TILL_POS_BIND
if [ -n "$TILL_POS_BIND" ]; then
  echo "POS will listen on $TILL_POS_BIND"
  docker compose up -d
fi
```

`TILL_POS_PORT` changes the published POS port. Set `TILL_POS_PUBLIC_URL` when a remote phone uses a reverse proxy, TLS termination or a different external port. A loopback-only installation needs an explicitly configured phone route before it can offer pairing.

In the browser, sign in, join your federation, connect your till to the registry, and pair your POS phone. Check the balance and review sales here. Confirm a Lightning address before enabling sweeps. A manual withdrawal as eCash uses a receiving wallet in the same federation.

## Backup and restore

The wallet, sales, card state, payout records and registry pairing are in the `wallet` volume at `/data`. The Compose project name determines which named volume Compose attaches. Back up the whole volume and your private admin credential. Store the archive encrypted and offline. Anyone holding this backup can access wallet funds.

Sync POS phones and settle queued offline payments first. End pending payouts and record the most recent sale and payout before the snapshot. Run from the directory where you installed the till. Choose a new filename for each archive. The shell will refuse to overwrite an existing archive when you use the commands below.

```sh
umask 077
set -o noclobber
docker compose stop till
docker compose run --rm --no-deps -T --entrypoint tar till -C /data -cpf - . > till-backup.tar
tar -tf till-backup.tar >/dev/null
docker compose start till
```

Check each command before continuing. Use a **fresh volume** in a separate Compose project with the same image version. Keep the original till stopped. An older backup can omit a paid invoice, customer top-up or card refund. When the backup predates wallet activity, keep the restored service stopped while you reconcile the missing activity with your records. Starting it can resume automatic payouts using the settings in the archive.

```sh
docker compose -p till-restore run --rm --no-deps -T --entrypoint sh till -ec \
  'test -z "$(ls -A /data)" && tar -C /data -xpf -' < till-backup.tar
```

After confirming the backup is current, keep the original instance offline and run `docker compose -p till-restore up -d` with the same version and admin credential. Open the admin page and check the federation, balance, payout destination, last sale and payout history against the snapshot. Verify a paired POS phone uses the restored API address.

## Test a local build

```sh
docker buildx build --load -t fedimintcard/till:review .
scripts/smoke-till-image.sh fedimintcard/till:review
```

The smoke check runs a temporary image instance on an isolated Docker network, verifies the admin login boundary, and tests a stopped-volume backup and restore with disposable data.
