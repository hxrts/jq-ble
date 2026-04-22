use blew::error::BlewError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum StartupStep {
    #[default]
    Pending,
    WaitingForPower,
    Retry,
    Started,
    Unsupported,
}

impl StartupStep {
    #[must_use]
    pub(crate) fn from_start_result<T>(result: Result<T, &BlewError>) -> Self {
        match result {
            Ok(_) => Self::Started,
            Err(BlewError::AlreadyAdvertising) => Self::Started,
            Err(_) => Self::Retry,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BleStartupState {
    pub(crate) l2cap_listener: StartupStep,
    pub(crate) gatt_service: StartupStep,
    pub(crate) advertising: StartupStep,
    pub(crate) scan: StartupStep,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdapterPowerState {
    PoweredOn,
    PoweredOff,
}

impl From<bool> for AdapterPowerState {
    fn from(powered: bool) -> Self {
        if powered {
            Self::PoweredOn
        } else {
            Self::PoweredOff
        }
    }
}

impl BleStartupState {
    pub(crate) fn central_power_changed(&mut self, state: AdapterPowerState) {
        self.scan = match state {
            AdapterPowerState::PoweredOn => StartupStep::Pending,
            AdapterPowerState::PoweredOff => StartupStep::WaitingForPower,
        };
    }

    pub(crate) fn peripheral_power_changed(&mut self, state: AdapterPowerState) {
        let next = match state {
            AdapterPowerState::PoweredOn => StartupStep::Pending,
            AdapterPowerState::PoweredOff => StartupStep::WaitingForPower,
        };
        if self.l2cap_listener != StartupStep::Unsupported {
            self.l2cap_listener = next;
        }
        self.gatt_service = next;
        self.advertising = next;
    }

    #[must_use]
    pub(crate) fn retry_due(&self) -> bool {
        [
            self.l2cap_listener,
            self.gatt_service,
            self.advertising,
            self.scan,
        ]
        .contains(&StartupStep::Retry)
    }
}
