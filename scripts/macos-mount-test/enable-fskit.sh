#!/bin/bash
# Enable the FUSE-T FSKit module without the System Settings GUI (for CI).
#
# System Settings > General > Login Items & Extensions > File System Extensions
# stores the approved modules in the FSKit settings group container
# (enabledModules.plist, an array of module bundle IDs). `pluginkit -e use`
# alone does not add the module there, and then no FSKit volume appears.
# This script adds the module to that list, then restarts the FSKit daemons
# so that they read it again. Exit 0 if the module is registered and listed.
ID=org.fuset.fskit-srv.module
SET="$HOME/Library/Group Containers/group.com.apple.fskit.settings"
echo "macOS $(sw_vers -productVersion)"
pluginkit -m -v -i $ID 2>&1 | sed 's/^/  pluginkit: /'
pluginkit -e use -i $ID 2>&1 | sed 's/^/  enable: /'

mkdir -p "$SET"
for f in enabledModules probeOrder; do
  p="$SET/$f.plist"
  [ -f "$p" ] || printf '%s\n' '<?xml version="1.0" encoding="UTF-8"?>' \
    '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' \
    '<plist version="1.0"><array/></plist>' > "$p"
  if ! plutil -p "$p" | grep -q "\"$ID\""; then
    plutil -insert 0 -string $ID "$p" || echo "  cannot write $p"
  fi
  echo "  $f: $(plutil -p "$p" | grep -c '=>') entries; $(plutil -p "$p" | grep -q "\"$ID\"" && echo "has $ID" || echo "NO $ID")"
done

# Make the daemons read the settings again (launchd starts them on demand).
killall fskit_agent 2>/dev/null
sudo -n killall fskitd 2>/dev/null
sleep 2
st=$(pluginkit -m -i $ID 2>/dev/null)
echo "  pluginkit state: ${st:-<not registered>}"
case "$st" in +*) ;; *) exit 1 ;; esac
plutil -p "$SET/enabledModules.plist" | grep -q "\"$ID\""
