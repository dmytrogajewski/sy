#!/usr/bin/env bash
set -euo pipefail

failure=0
device_path=${SY_NPU_DEVICE_PATH:-/dev/accel/accel0}
if [[ ! -e $device_path ]]; then
  echo 'NPU device missing: inspect sy-amdxdna-load.service and journalctl -k -b' >&2
  failure=1
fi
venv_root=${SY_RYZENAI_VENV_ROOT:-/opt/AMD/ryzenai/venv}
ort_candidates=("$venv_root"/lib/python*/site-packages/onnxruntime/capi/libonnxruntime.so.*)
if [[ ! -f ${ort_candidates[0]} ]]; then
  newer_candidates=("$venv_root"/onnxruntime/lib/libonnxruntime.so.* "$venv_root"/lib/python*/site-packages/onnxruntime/lib/libonnxruntime.so.*)
  newer_layout=false
  for candidate in "${newer_candidates[@]}"; do
    if [[ -f $candidate ]]; then newer_layout=true; break; fi
  done
  if [[ $newer_layout == true ]]; then
    echo 'Ryzen AI 1.8 layout detected; sy currently requires a qualified 1.7 VitisAI runtime adapter' >&2
  else
    echo "Ryzen AI runtime missing under $venv_root; verify the Fedora Ryzen AI RPM" >&2
  fi
  failure=1
else
  site_packages=${ort_candidates[0]%/onnxruntime/capi/*}
  if [[ ! -f $site_packages/voe-4.0-linux_x86_64/vaip_config.json ]]; then
    echo 'VitisAI config missing at the layout expected by sy; check Ryzen AI release compatibility' >&2
    failure=1
  fi
fi
xrt_lib_dir=${SY_XRT_LIB_DIR:-/opt/xilinx/xrt/lib}
if [[ ! -d $xrt_lib_dir ]]; then
  echo 'XRT libraries missing at /opt/xilinx/xrt/lib' >&2
  failure=1
fi
if (( failure )); then exit 1; fi
sy aiplane run --workload embed \
  --input '{"kind":"text","text":"Fedora upgrade smoke test"}' --json \
  | python3 -c '
import json, math, sys
result = json.load(sys.stdin)
vector = result.get("output", {}).get("vector", [])
if not vector or not all(math.isfinite(value) for value in vector):
    raise SystemExit("NPU embed returned no finite vector")
print(f"NPU embed passed ({len(vector)} dimensions)")
'
sy aiplane status --json | python3 -c '
import json, sys
status = json.load(sys.stdin)
if not status.get("daemon_running") or status.get("embed_backend") != "vitisai":
    raise SystemExit("AI daemon is down or embed is not using VitisAI")
'
echo "Host: $(. /etc/os-release; printf '%s %s' "$NAME" "$VERSION_ID")"
echo "Kernel: $(uname -r)"
echo 'AMD/XRT RPM versions:'
rpm -qa --qf '%{NAME} %{VERSION}-%{RELEASE}\n' \
  | awk 'tolower($1) ~ /(amdxdna|ryzenai|xrt|vitis|onnxruntime)/ { print }' \
  | sort
