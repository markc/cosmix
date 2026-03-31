# CosmixOS Alpine CT — Initial Setup

Bootstrap steps for a fresh Alpine Linux container (Incus CT) to serve as a CosmixOS node.

## 1. System update

```sh
apk update && apk upgrade
```

## 2. Essential packages

```sh
apk add bash nano rsync openssh
```

## 3. SSH server

```sh
ssh-keygen -A                    # generate host keys
mkdir -p /var/run/sshd
rc-service sshd start
rc-update add sshd default       # persist across reboots
```

## 4. User accounts

```sh
adduser sysadm
adduser markc
```

## 5. SSH key auth (root)

```sh
mkdir -p ~/.ssh && chmod 700 ~/.ssh
nano ~/.ssh/authorized_keys      # paste public key(s)
```

## 6. Set bash as default shell

Edit `/etc/passwd` — change `/bin/ash` to `/bin/bash` for desired users.
