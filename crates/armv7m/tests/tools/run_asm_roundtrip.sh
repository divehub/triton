#!/bin/sh
# Assembler round trip: generated assembly -> arm-none-eabi-as -> objdump -> armv7m decoder,
# compared instruction by instruction (see compare_objdump.py). Needs the GNU Arm toolchain
# (arm-none-eabi-as/objdump); not part of `cargo test`.
#
# Usage: run_asm_roundtrip.sh [work-dir]
set -e
here=$(cd "$(dirname "$0")" && pwd)
work=${1:-/tmp/armv7m-roundtrip}
mkdir -p "$work"
python3 "$here/gen_asm.py" > "$work/cases.s"
arm-none-eabi-as -mthumb -mcpu=cortex-m4 -mfpu=fpv4-sp-d16 -o "$work/cases.o" "$work/cases.s" 2>/dev/null
arm-none-eabi-objdump -d -M force-thumb,reg-names-raw "$work/cases.o" > "$work/cases.objdump"
python3 "$here/objdump_to_listing.py" "$work/cases.objdump" > "$work/cases.listing"
root=$(cd "$here/../../../.." && pwd)
"$root/cargo" run -q -p armv7m --release --example disasm_listing --target-dir target/cpu -- "$work/cases.listing" > "$work/cases.mine" 2> "$work/cases.log" || true
tail -n 3 "$work/cases.log"
python3 "$here/compare_objdump.py" "$work/cases.mine" "$work/cases.objdump" --show 60
