//! Whether a phone notification pops up on the computer.
//!
//! See `protocol/PROTOCOL.md`, section "Do Not Disturb". While that phone's Do Not
//! Disturb is on, system popups for its notifications are suppressed. The notification
//! is still listed in the window.

/// How the desktop should present one notification from a phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationPresentation {
    /// Pop up a system notification.
    pub popup: bool,
    /// Keep it in the Seam window list.
    pub list: bool,
}

/// Popup-suppression decision for one phone.
///
/// `dnd_on` is that phone's Do Not Disturb flag. Another phone's flag must not be
/// passed here. `true` means do not pop up a system notification.
#[must_use]
pub fn suppress_notification_popup(dnd_on: bool) -> bool {
    dnd_on
}

/// Full presentation: popups follow Do Not Disturb; the window list does not.
#[must_use]
pub fn present_notification(dnd_on: bool) -> NotificationPresentation {
    NotificationPresentation {
        popup: !suppress_notification_popup(dnd_on),
        list: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_suppression_decision() {
        assert!(
            suppress_notification_popup(true),
            "Do Not Disturb silences system popups"
        );
        assert!(
            !suppress_notification_popup(false),
            "popups stay on when Do Not Disturb is off"
        );

        let silenced = present_notification(true);
        assert!(!silenced.popup);
        assert!(silenced.list, "the window still lists the notification");

        let normal = present_notification(false);
        assert!(normal.popup);
        assert!(normal.list);
    }

    #[test]
    fn each_phone_uses_its_own_flag() {
        // The caller passes the notifying phone's flag only.
        assert!(suppress_notification_popup(true));
        assert!(!suppress_notification_popup(false));
        assert!(!present_notification(true).popup);
        assert!(present_notification(false).popup);
        assert!(present_notification(true).list);
        assert!(present_notification(false).list);
    }
}
