// Omacloud in the bar: a cloud that says how sync stands, as iCloud's does in
// a menu bar. The daemon writes how it stands to a small file in the
// session's runtime directory whenever that changes, and every half minute,
// and this watches the file, so nothing here talks to the storage. A line
// gone old means the daemon is gone.
import QtQuick
import Quickshell
import Quickshell.Io
import qs.Ui

BarWidget {
  id: root
  moduleName: "ferdousbhai.omacloud"
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  // { state, text, at } as `omacloud watch` last wrote it
  property var said: ({})
  readonly property bool fresh: said.at !== undefined && Date.now() / 1000 - said.at < 120
  readonly property string state_: fresh && said.state ? said.state : "stopped"

  function read(text) {
    try { root.said = JSON.parse(text) } catch (e) { root.said = {} }
  }

  FileView {
    id: file
    path: Quickshell.env("XDG_RUNTIME_DIR") + "/omacloud/bar.json"
    watchChanges: true
    printErrors: false
    onLoaded: root.read(file.text())
    onLoadFailed: root.said = {}
    onFileChanged: reload()
  }
  // the file appearing for the first time isn't a change to it, and one
  // that stops changing has to be noticed as old
  Timer {
    interval: 30000
    running: true
    repeat: true
    onTriggered: file.reload()
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    // cloud with a tick, with arrows, paused, with a warning, and crossed out
    text: root.state_ === "synced" ? "󰅠"
      : root.state_ === "syncing" ? "󰘿"
      : root.state_ === "paused" ? "󰅟"
      : root.state_ === "attention" ? "󰧠"
      : "󰅤"
    active: root.state_ === "attention"
    dimmed: root.state_ === "stopped" || root.state_ === "paused"
    horizontalMargin: 8
    tooltipText: "Omacloud: " + (root.fresh && root.said.text ? root.said.text : "not running")
    onPressed: function(mouseButton) {
      if (root.bar) root.bar.run("omacloud-app")
    }
  }
}
