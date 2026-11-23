// A stand-in for the Omarchy shell's BarWidget, with what Omacloud's widget uses of it: scripts/bar-widget.sh
// lints and runs the widget against it, where there is no Omarchy shell.
import QtQuick

Item {
  property string moduleName: ""
  property var bar: null
}
