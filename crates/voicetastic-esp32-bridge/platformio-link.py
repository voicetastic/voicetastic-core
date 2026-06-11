# PlatformIO pre-build hook: cross-compile voicetastic-esp32-bridge and link
# its static archive + C header into the firmware.
#
# TEMPLATE / UNTESTED ON HARDWARE. It encodes the slice-1 plan; the Xtensa
# build + link still needs validating with espup on a real t-deck-tft. Copy
# into the firmware repo and reference from platformio.ini, e.g.:
#
#   [env:t-deck-tft]
#   extra_scripts = +<../../../crates/voicetastic-esp32-bridge/platformio-link.py>
#
# Requires:
#   - espup-installed Xtensa Rust toolchain (cargo build --target xtensa-esp32s3-espidf)
#   - a sibling checkout of voicetastic-core; override with VT_CORE_DIR env var.
#
# Open question this script will surface on first run: whether the
# `xtensa-esp32s3-espidf` (std) archive links cleanly here, or whether a
# `no_std` core subset / `xtensa-esp32s3-none-elf` target is needed because
# esp-idf-sys wants to own the ESP-IDF build. See README.md.

import os
import subprocess

Import("env")  # noqa: F821  (injected by PlatformIO/SCons)

TARGET = "xtensa-esp32s3-espidf"
CRATE = "voicetastic-esp32-bridge"

project_dir = env["PROJECT_DIR"]  # noqa: F821
core_dir = os.environ.get(
    "VT_CORE_DIR",
    os.path.normpath(os.path.join(project_dir, "..", "voicetastic-core")),
)
if not os.path.isdir(core_dir):
    raise SystemExit(
        f"[vt-core] voicetastic-core not found at {core_dir}; "
        "set VT_CORE_DIR or place it as a sibling of the firmware checkout."
    )

print(f"[vt-core] building {CRATE} ({TARGET}) from {core_dir}")
subprocess.check_call(
    [
        "cargo", "build", "--release",
        "-p", CRATE,
        "--target", TARGET,
    ],
    cwd=core_dir,
)

lib_dir = os.path.join(core_dir, "target", TARGET, "release")
lib_path = os.path.join(lib_dir, "libvoicetastic_esp32_bridge.a")
include_dir = os.path.join(core_dir, "crates", "voicetastic-esp32-bridge", "include")
if not os.path.isfile(lib_path):
    raise SystemExit(f"[vt-core] expected archive missing: {lib_path}")

env.Append(CPPPATH=[include_dir])           # noqa: F821  vt_core header
env.Append(LIBPATH=[lib_dir])               # noqa: F821
env.Append(LIBS=["voicetastic_esp32_bridge"])  # noqa: F821  links the .a
print(f"[vt-core] linked {lib_path}")
