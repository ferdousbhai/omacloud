// Omacloud in the bar: a cloud that says how sync stands, as iCloud's does in
// a menu bar. The daemon writes how it stands to a small file in the
// session's runtime directory whenever that changes, and every half minute,
// and this watches the file, so nothing here talks to the storage, and
// nothing here polls: each line arrives as the file's change. A line gone
// old means the daemon is gone.
import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons as Commons
import qs.Ui

BarWidget {
  id: root
  moduleName: "ferdousbhai.omacloud"
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  // The daemon says the same again every 30 seconds; a line this old, in
  // seconds, was left by one that was killed.
  readonly property int oldAfter: 120
  readonly property string folder: Quickshell.env("XDG_RUNTIME_DIR") + "/omacloud"

  // { state, text, at } as `omacloud watch` last wrote it
  property var said: ({})
  property bool fresh: false
  readonly property string state_: fresh && said.state ? said.state : "stopped"

  function read(text) {
    try { root.said = JSON.parse(text) } catch (e) { root.said = {} }
    var left = root.said.at !== undefined ? root.said.at + root.oldAfter - Date.now() / 1000 : 0
    root.fresh = left > 0
    // Nothing tells of a daemon that was killed, so the line's own age has
    // to: one timer, set for the moment this line goes old, which a living
    // daemon's next line sets again long before it runs out. A daemon that
    // said it stopped has no more to say, and nothing waits for it.
    if (root.fresh && root.said.state !== "stopped") {
      old.interval = Math.ceil(left * 1000)
      old.restart()
    } else {
      old.stop()
    }
  }

  // The watch tells of the file changing and, through its folder, of the
  // file arriving or being replaced, but only in a folder that is there as
  // the watch is set. The daemon makes the folder with its first line,
  // which at login may be after the bar starts: so the folder is made here
  // first, once, and reading the file again sets the watch anew.
  Process {
    running: true
    command: ["mkdir", "-p", "-m", "700", root.folder]
    onExited: file.reload()
  }
  FileView {
    id: file
    path: root.folder + "/bar.json"
    watchChanges: true
    printErrors: false
    onLoaded: root.read(file.text())
    onLoadFailed: root.read("")
    onFileChanged: reload()
  }
  // Runs out only when no line came in time. It reads the file once more
  // before believing that: should the watch itself have been lost (the
  // daemon replaces the file with each line, and a watch on the new one
  // can be refused), a living daemon's line is found and waited on again.
  Timer {
    id: old
    onTriggered: file.reload()
  }

  // The widget's words, in the language picked in Omarchy and followed as it
  // changes (i18n/en.json and its translations). The daemon writes its line
  // in English, from a set of its own: each is a key here, and one this
  // doesn't know is shown as the daemon wrote it.
  Commons.Translations { id: strings; directory: Qt.resolvedUrl("i18n") }

  // The bar's tooltip may read its text as markup, and the line is the
  // daemon's: angle brackets are shown as look-alikes, never as a tag.
  function plain(text) {
    return String(text).replace(/</g, "\u2039").replace(/>/g, "\u203a")
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
    // its margins are the shell's own (Style), as every bar widget's
    tooltipText: strings.t("tooltip").arg(root.plain(root.fresh && root.said.text
      ? strings.t("daemon." + root.said.text, root.said.text)
      : strings.t("daemon.not running")))
    onPressed: function(mouseButton) {
      if (root.bar) root.bar.run("omacloud open")
    }
  }
}
