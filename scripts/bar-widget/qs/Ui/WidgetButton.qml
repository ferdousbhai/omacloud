// A stand-in for the Omarchy shell's WidgetButton, with what Omacloud's widget uses of it (see BarWidget.qml).
import QtQuick

Item {
  property var bar: null
  property string text: ""
  property bool active: false
  property bool dimmed: false
  property int horizontalMargin: 0
  property string tooltipText: ""
  signal pressed(int mouseButton)
}
