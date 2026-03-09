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

### Output

The generated package is written to `rs-matterd/dist/`.

Install it on the Pi with:

```sh
sudo apt install ./rs-matterd/dist/rs-matterd_<version>_armhf.deb
sudo systemctl enable --now rs-matterd
```
