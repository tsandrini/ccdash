# Security

Anyone holding a panel token can read your sessions, answer their prompts,
start and stop sessions in your projects and, with the channel, prompt
them. Treat a token like an SSH key and keep the hub on your LAN or VPN.

1. [Tokens](#1-tokens)
2. [Pairing a device](#2-pairing-a-device)
3. [What reaches the hub](#3-what-reaches-the-hub)

## 1. Tokens

| Token            | Held by                         | Checked as                                              |
| ---------------- | ------------------------------- | ------------------------------------------------------- |
| bootstrap        | `tokenFile` on the hub          | compared in constant time                               |
| device           | each paired browser             | random, stored as SHA-256, revocable                    |
| collector        | each workstation (`collectors`) | names the machine; only `/collect` and `/sync/` take it |
| notification key | one prompt's buttons            | random, dies with the prompt                            |

Rotating `tokenFile` (and restarting the hub) signs out only the devices
still on the bootstrap token.

## 2. Pairing a device

```sh
sudo -u ccdash ccdash url --hub --config /etc/ccdash/hub.toml --qr
```

or "Connect a phone" in Settings on a paired browser. Every link carries a
token minted for that one device: good for an hour until the link is
opened, then for as long as the device keeps it.

Settings › Devices lists them with their browser and last use; rename one
in place, or revoke it to sign it out at its next request and end its push
notifications. A device still on the bootstrap token sees "shared token"
there and can take one of its own.

## 3. What reaches the hub

- The API under `/api/v1` needs a bearer token for everything.
- `/act/<key>/<action>` takes a notification button's one-time key; the
  access token never goes into a notification.
- `/collect` and `/sync/` take a collector's token; `/sync/` paths are
  checked to stay inside the synced directories.
- The OpenTelemetry receiver on a workstation takes no token, so the
  collector binds it to loopback only.
- Nothing leaves the hub except Web Push to subscribed devices, encrypted
  for the device: the push service sees only ciphertext.
- The hub keeps the synced `~/.claude` as plain files: put its directory in
  an encrypted backup ([state sync](state-sync.md#7-on-the-hub)).
