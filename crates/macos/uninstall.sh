#!/bin/bash
# Remove the screentime client from this Mac (sudo for a system-wide install). Keeps nothing.
set -uo pipefail
if [ "$(id -u)" = 0 ]; then
  launchctl bootout system/org.screentime.agent 2>/dev/null
  for u in $(who | awk '$2=="console"{print $1}' | sort -u); do launchctl bootout "gui/$(id -u "$u")/org.screentime.menu" 2>/dev/null; done
  rm -f /Library/LaunchDaemons/org.screentime.agent.plist /Library/LaunchAgents/org.screentime.menu.plist
  rm -rf "/Library/Application Support/screentime"
else
  for l in org.screentime.agent org.screentime.menu; do
    launchctl bootout "gui/$(id -u)/$l" 2>/dev/null
    rm -f "$HOME/Library/LaunchAgents/$l.plist"
  done
  rm -rf "$HOME/Library/Application Support/screentime"
fi
pkill -x ScreenTimeMenu 2>/dev/null
echo "Removed."
