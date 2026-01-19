import QtQuick
import QtQuick.Layouts
import org.kde.plasma.plasmoid
import org.kde.plasma.components as PlasmaComponents
import org.kde.kirigami as Kirigami
import org.kde.plasma.plasma5support as Plasma5Support

PlasmoidItem {
    id: root

    property bool grabMode: true

    // Force compact representation only, no popup
    preferredRepresentation: compactRepresentation
    fullRepresentation: compactRepresentation

    toolTipMainText: grabMode ? "Mode: GRAB" : "Mode: PASSIVE"
    toolTipSubText: grabMode ? "Correct first key (~1ms latency)" : "Zero latency (gaming)"

    compactRepresentation: Item {
        id: compactItem

        Layout.fillWidth: false
        Layout.fillHeight: true
        Layout.minimumWidth: button.width
        Layout.preferredWidth: button.width

        Rectangle {
            id: button
            anchors.centerIn: parent
            width: label.implicitWidth + Kirigami.Units.smallSpacing * 4
            height: parent.height > 0 ? Math.min(parent.height, label.implicitHeight + Kirigami.Units.smallSpacing * 2) : label.implicitHeight + Kirigami.Units.smallSpacing * 2

            radius: 3
            color: root.grabMode ? "#27ae60" : "#c0392b"

            PlasmaComponents.Label {
                id: label
                anchors.centerIn: parent
                text: root.grabMode ? "KB" : "GM"
                font.bold: true
            }
        }

        MouseArea {
            anchors.fill: parent
            onClicked: {
                root.toggleMode()
            }
        }
    }

    Plasma5Support.DataSource {
        id: executable
        engine: "executable"
        connectedSources: []

        onNewData: (sourceName, data) => {
            var stdout = data["stdout"]
            if (stdout.indexOf("grab") !== -1) {
                root.grabMode = true
            } else if (stdout.indexOf("passive") !== -1) {
                root.grabMode = false
            }
            disconnectSource(sourceName)
        }

        function exec(cmd) {
            if (cmd) {
                connectSource(cmd)
            }
        }
    }

    function toggleMode() {
        executable.exec("dbus-send --session --print-reply --dest=org.kblayout.Daemon /org/kblayout/Daemon org.kblayout.Daemon.ToggleMode")
        modeCheckTimer.restart()
    }

    function checkMode() {
        executable.exec("dbus-send --session --print-reply --dest=org.kblayout.Daemon /org/kblayout/Daemon org.kblayout.Daemon.GetMode")
    }

    Timer {
        id: modeCheckTimer
        interval: 300
        onTriggered: checkMode()
    }

    Timer {
        id: periodicCheck
        interval: 5000
        running: true
        repeat: true
        onTriggered: checkMode()
    }

    Component.onCompleted: checkMode()
}
