## Archived rs-matterd packaging

This directory preserves the original `rs-matterd` packaging and build
automation that previously lived at the repository root.

The build flow remains:

1. Bootstrap an Ubuntu x86 build host.
2. Clone upstream `rs-matter` into `build/upstream/rs-matter/`.
3. Copy in the local `rs-matterd` overlay crate.
4. Cross-build `armv7-unknown-linux-gnueabihf`.
5. Assemble `dist/rs-matterd_<version>_armhf.deb`.

`rs-matterd` is archived as a legacy experiment and is no longer the active
HomeNode Server runtime.

### Entry point

```sh
./archive/rs-matterd/scripts/rs-matter/build-deb.sh
```

Useful overrides:

```sh
RS_MATTER_UPSTREAM_URL=https://github.com/project-chip/rs-matter.git \
RS_MATTER_UPSTREAM_REF=main \
DEB_MAINTAINER="Your Name <you@example.com>" \
./archive/rs-matterd/scripts/rs-matter/build-deb.sh
```

Runtime overrides through `/etc/default/rs-matterd`:

```sh
RS_MATTERD_STATE_DIR=/var/lib/rs-matterd
RS_MATTERD_SETUP_PIN=20202021
RS_MATTERD_DISCRIMINATOR=3840
```

### Output

The generated package is written to `archive/rs-matterd/dist/`.

Install it on the Pi with:

```sh
sudo apt install ./archive/rs-matterd/dist/rs-matterd_<version>_armhf.deb
sudo systemctl enable --now rs-matterd
```
