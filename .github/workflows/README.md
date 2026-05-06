# CI notes

`ci.yml` runs on `ubuntu-latest` only. Four jobs: `fmt`, `build`, `test`,
`clippy`. All four use the stable Rust toolchain.

## Why Linux-only

The `backend-xen` crate links against libxl (Xen Light), which is a
Linux-only library. macOS and Windows runners cannot build the workspace
at all. There is also no Xen hypervisor on GitHub-hosted runners — they
are themselves VMs, not Xen dom0s.

## Why the tests still pass without a Xen runtime

`backend-xen` initialises through `Ctx::new` / `XenBackend::new`, which
call `libxl_ctx_alloc`. On any non-dom0 host that call fails. The unit
tests (e.g. `xen_backend_new_fails_on_non_xen_host`) assert that
failure, so CI is the same exercise as a developer laptop: the
backend-init path errors out, and the test that expects an error
passes.

## Local validation without Linux

If you don't have a Linux box, the cheapest path is a Linux VM (or
container) with `libxen-dev pkg-config libclang-dev` installed. From
there, `cargo test --workspace` reproduces what CI runs. The non-Xen
backend-init tests behave identically inside any plain Linux VM —
no dom0 needed.
