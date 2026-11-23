// The shell scripts/bar-widget.sh runs the widget in: it says how the widget stands, and what its tooltip reads,
// each time either changes.
import QtQuick
import Quickshell

ShellRoot {
  Widget {
    id: widget
    readonly property string shown: state_ + " | " + children[0].tooltipText
    onShownChanged: console.log("standing: " + shown)
    Component.onCompleted: console.log("standing: " + shown)
  }
}
