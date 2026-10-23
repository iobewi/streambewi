//! Bench qualification only (#31): read-only snapshots of the OTG core's registers while the device is on the
//! bus, to tell a host that stopped talking from a device that stopped answering. Nothing is written and no
//! register with a read side effect is touched (not the FIFOs). Bounded: one line every 500 ms for the first
//! 40 s after the product enables USB, then one every 10 s.

use embassy_time::{Duration, Instant, Timer};
use embassy_usb_synopsys_otg::otg_v1::Otg;
use esp_hal::peripherals::USB_FS;
use streambewi::usb_qual::{Ev, first, report};

#[embassy_executor::task]
pub async fn probe() {
    // SAFETY: the register block of the OTG FS peripheral owned by the USB driver; this only reads.
    let regs = unsafe { Otg::from_ptr(USB_FS::PTR.cast_mut().cast::<()>()) };
    let enabled_at = loop {
        if let Some(t) = first(Ev::Enabled) {
            break t;
        }
        Timer::after(Duration::from_millis(100)).await;
    };
    let mut lines = 0u32;
    loop {
        let since = Instant::now().as_millis() as u32 - enabled_at;
        let period = if since < 40_000 { 500 } else { 10_000 };
        // Endpoint 1 is the MSC bulk pair (IN and OUT), endpoint 0 the control pipe.
        log::info!(
            "usb-regs t={} dsts={:08x} dctl={:08x} gintsts={:08x} in1={:08x}/{:08x}/{:08x}/{:08x} out1={:08x}/{:08x}/{:08x} in0={:08x} out0={:08x}",
            Instant::now().as_millis(),
            regs.dsts().read().0,
            regs.dctl().read().0,
            regs.gintsts().read().0,
            regs.diepctl(1).read().0,
            regs.dieptsiz(1).read().0,
            regs.diepint(1).read().0,
            regs.dtxfsts(1).read().0,
            regs.doepctl(1).read().0,
            regs.doeptsiz(1).read().0,
            regs.doepint(1).read().0,
            regs.diepctl(0).read().0,
            regs.doepctl(0).read().0,
        );
        lines += 1;
        if lines % 10 == 0 {
            report("periodic");
        }
        Timer::after(Duration::from_millis(period)).await;
    }
}
