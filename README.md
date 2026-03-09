# HomeNodeServer

HomeNodeServer is the main repository for the HomeNode runtime and its build
automation.

## rs-matterd

`rs-matterd` is the Matter-related component of HomeNodeServer. The
`rs-matterd/` directory contains:

- Ubuntu build host bootstrap scripts
- upstream `rs-matter` checkout automation
- the local `rs-matterd` overlay crate
- Debian packaging files for Raspberry Pi Zero 2 (`armhf`, Debian Trixie)

Build the package with:

```sh
./rs-matterd/scripts/rs-matter/build-deb.sh
```

The script produces a local Debian package under `rs-matterd/dist/`.
