use std::io;

use crate::cmdline::KernelCommandLine;

pub(crate) use qbootctl::Slot;

#[derive(Clone, Debug)]
pub(crate) struct Slots {
    inner: qbootctl::BootControl,
}

impl Slots {
    pub(crate) fn new(cmdline: KernelCommandLine) -> Self {
        Self {
            inner: qbootctl::BootControl::new(cmdline_slot(&cmdline)),
        }
    }

    pub(crate) fn slot_count(&self) -> io::Result<usize> {
        self.inner.slot_count()
    }

    pub(crate) fn has_slot(&self, partition: &str) -> io::Result<bool> {
        self.inner.has_slot(partition)
    }

    pub(crate) fn current_slot(&self) -> io::Result<Option<Slot>> {
        self.inner.current_slot()
    }

    pub(crate) fn is_slot_successful(&self, slot: Slot) -> io::Result<Option<bool>> {
        self.inner.is_slot_successful(slot)
    }

    pub(crate) fn is_slot_unbootable(&self, slot: Slot) -> io::Result<Option<bool>> {
        self.inner.is_slot_unbootable(slot)
    }

    pub(crate) fn set_active(&self, slot: Slot) -> io::Result<()> {
        self.inner.set_active(slot)
    }
}

fn cmdline_slot(cmdline: &KernelCommandLine) -> Option<Slot> {
    cmdline
        .value("androidboot.slot_suffix")
        .or_else(|| cmdline.value("slot_suffix"))
        .and_then(Slot::parse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cmdline_slot_suffix() {
        let cmdline = KernelCommandLine::parse("foo androidboot.slot_suffix=_b bar");

        assert_eq!(cmdline_slot(&cmdline), Some(Slot::B));
    }
}
