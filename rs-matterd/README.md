## rs-matterd packaging

This directory contains the `rs-matter` packaging and build automation for
`HomeNodeServer`.

The build flow is:

1. Bootstrap an Ubuntu x86 build host.
2. Clone upstream `rs-matter` into `build/upstream/rs-matter/`.
3. Copy in the local `rs-matterd` overlay crate.
4. Cross-build `armv7-unknown-linux-gnueabihf`.
5. Assemble `dist/rs-matterd_<version>_armhf.deb`.

`rs-matterd` is intended to become the `rs-matter` runtime component inside
`HomeNodeServer`.

The current daemon is a minimal persistent Ethernet Matter node:

- it restores state from `/var/lib/rs-matterd`
- it starts uncommissioned if no persisted fabric exists
- it exposes a fixed default setup PIN and discriminator unless overridden
- it can be commissioned remotely from a commissioner such as `chip-tool`

### Entry point

```sh
./rs-matterd/scripts/rs-matter/build-deb.sh
```

Useful overrides:

```sh
RS_MATTER_UPSTREAM_URL=https://github.com/project-chip/rs-matter.git \
RS_MATTER_UPSTREAM_REF=main \
DEB_MAINTAINER="Your Name <you@example.com>" \
./rs-matterd/scripts/rs-matter/build-deb.sh
```

Runtime overrides through `/etc/default/rs-matterd`:

```sh
RS_MATTERD_STATE_DIR=/var/lib/rs-matterd
RS_MATTERD_SETUP_PIN=20202021
RS_MATTERD_DISCRIMINATOR=3840
```

### Output

The generated package is written to `rs-matterd/dist/`.

Install it on the Pi with:

```sh
sudo apt install ./rs-matterd/dist/rs-matterd_<version>_armhf.deb
sudo systemctl enable --now rs-matterd
```

### Initial commissioning

If no persisted fabric exists yet, `rs-matterd` starts commissionable and logs
the setup information to journald.

On a commissioner host with `chip-tool`, typical commands are:

```sh
chip-tool pairing onnetwork 1234 20202021
chip-tool pairing onnetwork-long 1234 20202021 3840
```

If discovery does not work, pair directly by IP:

```sh
chip-tool pairing ethernet 1234 20202021 3840 <pi-ip> 5540
```
