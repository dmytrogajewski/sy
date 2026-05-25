# Ryzen AI environment setup. Owned by sy (configs/profile.d/ryzenai.sh,
# installed by scripts/install-system-npu.sh). BUG-20260524-2235.
#
# Upstream `ryzenai-1.7.1-1.fc43`'s `/etc/profile.d/ryzenai.sh` sources
# `/opt/xilinx/xrt/setup.sh` unguarded and unredirected — every
# interactive shell then prints a four-line banner (XILINX_XRT / PATH /
# LD_LIBRARY_PATH / PYTHONPATH) and re-prepends those dirs, so $PATH
# bloats by 3 XRT entries per shell nest. This file is the silent +
# idempotent shape (recovered from the .rpmsave the rpm upgrade left
# behind). Re-run `make install-system-npu` after any ryzenai rpm
# refresh to restore it.

export RYZEN_AI_INSTALLATION_PATH=/opt/AMD/ryzenai

if [ -z "$XILINX_XRT" ] && [ -f /opt/xilinx/xrt/setup.sh ]; then
    . /opt/xilinx/xrt/setup.sh >/dev/null
fi

case ":$PATH:" in
    *":${RYZEN_AI_INSTALLATION_PATH}/venv/bin:"*) ;;
    *) [ -d "${RYZEN_AI_INSTALLATION_PATH}/venv/bin" ] \
        && PATH="${RYZEN_AI_INSTALLATION_PATH}/venv/bin:$PATH" ;;
esac
export PATH
