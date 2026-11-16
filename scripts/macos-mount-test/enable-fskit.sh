#!/bin/bash
# Try to enable the FUSE-T FSKit extension without the System Settings GUI
# (for CI runners). Prints what it finds; exit 0 if the extension looks enabled.
ID=org.fuset.fskit-srv.module
echo "macOS $(sw_vers -productVersion)"
pluginkit -m -v -i $ID 2>&1 | sed 's/^/  pluginkit: /'
pluginkit -e use -i $ID 2>&1 | sed 's/^/  enable: /'
sleep 2
st=$(pluginkit -m -i $ID 2>/dev/null)
echo "  state: ${st:-<not registered>}"
case "$st" in +*) exit 0 ;; *) exit 1 ;; esac
