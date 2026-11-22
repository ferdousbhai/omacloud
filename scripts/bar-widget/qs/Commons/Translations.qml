// A stand-in for the Omarchy shell's Translations, with what Omacloud's widget uses of it (see ../Ui/BarWidget.qml):
// the English catalog of the plugin, read from its i18n directory and read again as it changes, a string missing
// from it given as the fallback, else as its key. The shell's own also follows the language picked in Omarchy.
import QtQuick
import Quickshell.Io

QtObject {
  id: root

  property var directory: ""
  property var strings: ({})

  function t(key, fallback) {
    var value = root.strings[key]
    if (value && typeof value === "object") value = value.other
    if (typeof value === "string" && value.length > 0) return value
    return fallback !== undefined ? fallback : key
  }

  property FileView english: FileView {
    path: String(root.directory).replace(/^file:\/\//, "").replace(/\/+$/, "") + "/en.json"
    watchChanges: true
    blockLoading: true
    printErrors: false
    onLoaded: {
      try { root.strings = JSON.parse(text()) } catch (e) { root.strings = {} }
    }
    onFileChanged: reload()
  }
}
