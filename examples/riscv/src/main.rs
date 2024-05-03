#![no_std]
#![no_main]
#![deny(warnings)]

#[macro_use]
extern crate log;

extern crate alloc;
extern crate opensbi_rt;

use alloc::vec;
use core::ptr::NonNull;
use fdt::{node::FdtNode, standard_nodes::Compatible, Fdt};
use log::LevelFilter;
use virtio_drivers::{
    device::{
        blk::VirtIOBlk,
        gpu::VirtIOGpu,
        input::VirtIOInput,
        sound::{PcmFormats, PcmRate, VirtIOSound},
    },
    transport::{
        mmio::{MmioTransport, VirtIOHeader},
        DeviceType, Transport,
    }
};
use virtio_impl::HalImpl;

mod virtio_impl;

#[cfg(feature = "tcp")]
mod tcp;

const NET_QUEUE_SIZE: usize = 16;

#[no_mangle]
extern "C" fn main(_hartid: usize, device_tree_paddr: usize) {
    log::set_max_level(LevelFilter::Info);
    init_dt(device_tree_paddr);
    info!("test end");
}

fn init_dt(dtb: usize) {
    info!("device tree @ {:#x}", dtb);
    // Safe because the pointer is a valid pointer to unaliased memory.
    let fdt = unsafe { Fdt::from_ptr(dtb as *const u8).unwrap() };
    walk_dt(fdt);
}

fn walk_dt(fdt: Fdt) {
    for node in fdt.all_nodes() {
        if let Some(compatible) = node.compatible() {
            if compatible.all().any(|s| s == "virtio,mmio") {
                virtio_probe(node);
            }
        }
    }
}

fn virtio_probe(node: FdtNode) {
    if let Some(reg) = node.reg().and_then(|mut reg| reg.next()) {
        let paddr = reg.starting_address as usize;
        let size = reg.size.unwrap();
        let vaddr = paddr;
        info!("walk dt addr={:#x}, size={:#x}", paddr, size);
        info!(
            "Device tree node {}: {:?}",
            node.name,
            node.compatible().map(Compatible::first),
        );
        let header = NonNull::new(vaddr as *mut VirtIOHeader).unwrap();
        match unsafe { MmioTransport::new(header) } {
            Err(e) => warn!("Error creating VirtIO MMIO transport: {}", e),
            Ok(transport) => {
                info!(
                    "Detected virtio MMIO device with vendor id {:#X}, device type {:?}, version {:?}",
                    transport.vendor_id(),
                    transport.device_type(),
                    transport.version(),
                );
                virtio_device(transport);
            }
        }
    }
}

fn virtio_device(transport: impl Transport) {
    match transport.device_type() {
        DeviceType::Block => virtio_blk(transport),
        DeviceType::GPU => virtio_gpu(transport),
        DeviceType::Input => virtio_input(transport),
        DeviceType::Network => virtio_net(transport),
        DeviceType::Sound => virtio_sound(transport),
        t => warn!("Unrecognized virtio device: {:?}", t),
    }
}

fn virtio_blk<T: Transport>(transport: T) {
    let mut blk = VirtIOBlk::<HalImpl, T>::new(transport).expect("failed to create blk driver");
    let mut input = vec![0xffu8; 512];
    let mut output = vec![0; 512];
    for i in 0..32 {
        for x in input.iter_mut() {
            *x = i as u8;
        }
        blk.write_blocks(i, &input).expect("failed to write");
        blk.read_blocks(i, &mut output).expect("failed to read");
        assert_eq!(input, output);
    }
    info!("virtio-blk test finished");
}

fn virtio_gpu<T: Transport>(transport: T) {
    let mut gpu = VirtIOGpu::<HalImpl, T>::new(transport).expect("failed to create gpu driver");
    let (width, height) = gpu.resolution().expect("failed to get resolution");
    let width = width as usize;
    let height = height as usize;
    info!("GPU resolution is {}x{}", width, height);
    let fb = gpu.setup_framebuffer().expect("failed to get fb");
    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            fb[idx] = x as u8;
            fb[idx + 1] = y as u8;
            fb[idx + 2] = (x + y) as u8;
        }
    }
    gpu.flush().expect("failed to flush");
    //delay some time
    info!("virtio-gpu show graphics....");
    for _ in 0..10000 {
        for _ in 0..100000 {
            unsafe {
                core::arch::asm!("nop");
            }
        }
    }

    info!("virtio-gpu test finished");
}

fn virtio_input<T: Transport>(transport: T) {
    //let mut event_buf = [0u64; 32];
    let mut _input =
        VirtIOInput::<HalImpl, T>::new(transport).expect("failed to create input driver");
    // loop {
    //     input.ack_interrupt().expect("failed to ack");
    //     info!("mouse: {:?}", input.mouse_xy());
    // }
    // TODO: handle external interrupt
}

fn virtio_net<T: Transport>(transport: T) {
    #[cfg(not(feature = "tcp"))]
    {
        let mut net =
            virtio_drivers::device::net::VirtIONetRaw::<HalImpl, T, NET_QUEUE_SIZE>::new(transport)
                .expect("failed to create net driver");
        info!("MAC address: {:02x?}", net.mac_address());

        let mut buf = [0u8; 2048];
        let (hdr_len, pkt_len) = net.receive_wait(&mut buf).expect("failed to recv");
        info!(
            "recv {} bytes: {:02x?}",
            pkt_len,
            &buf[hdr_len..hdr_len + pkt_len]
        );
        net.send(&buf[..hdr_len + pkt_len]).expect("failed to send");
        info!("virtio-net test finished");
    }

    #[cfg(feature = "tcp")]
    {
        const NET_BUFFER_LEN: usize = 2048;
        let net = virtio_drivers::device::net::VirtIONet::<HalImpl, T, NET_QUEUE_SIZE>::new(
            transport,
            NET_BUFFER_LEN,
        )
        .expect("failed to create net driver");
        info!("MAC address: {:02x?}", net.mac_address());
        tcp::test_echo_server(net);
    }
}

fn virtio_sound<T: Transport>(transport: T) {
    let mut sound =
        VirtIOSound::<HalImpl, T>::new(transport).expect("failed to create sound driver");
    let output_streams = sound.output_streams();
    if output_streams.len() > 0 {
        let output_stream_id = *output_streams.first().unwrap();
        let rates = sound.rates_supported(output_stream_id).unwrap();
        let formats = sound.formats_supported(output_stream_id).unwrap();
        let channel_range = sound.channel_range_supported(output_stream_id).unwrap();
        let features = sound.features_supported(output_stream_id).unwrap();

        let rate = if rates.contains(PcmRate::VIRTIO_SND_PCM_RATE_44100) {
            PcmRate::VIRTIO_SND_PCM_RATE_44100
        } else {
            PcmRate::VIRTIO_SND_PCM_RATE_32000
        };
        let format = if formats.contains(PcmFormats::VIRTIO_SND_PCM_FMT_U8) {
            PcmFormats::VIRTIO_SND_PCM_FMT_U8
        } else {
            PcmFormats::VIRTIO_SND_PCM_FMT_U32
        };
        let channel = if channel_range.contains(&2) {
            2 as u8
        } else {
            *channel_range.start()
        };
        sound
            .pcm_set_params(
                output_stream_id,
                2 * 441,
                441,
                features,
                channel,
                format,
                rate,
            )
            .expect("pcm_set_params error");
        sound
            .pcm_prepare(output_stream_id)
            .expect("pcm_prepare error");
        sound.pcm_start(output_stream_id).expect("pcm_start error");
        let music = include_bytes!("../sun_also_rises_44100Hz_u8_stereo.raw");
        info!("[sound device] music len is {} bytes.", music.len());
        // let xfer_times = if music.len() % 64 == 0 {
        //     music.len() / 64
        // } else {
        //     music.len() / 64 + 1
        // };
        // for i in 0..xfer_times {
        //     let start_byte = 64 * i;
        //     let msg = format!("pcm_xfer at xfer time {} failed", i);
        //     // the last xfer
        //     if i == xfer_times - 1 {
        //         sound.pcm_xfer(output_stream_id, &music[start_byte..]).expect(&msg);
        //         break;
        //     }
        //     let end_byte = 64 * (i + 1);
        //     sound.pcm_xfer(output_stream_id, &music[start_byte..end_byte]).expect(&msg);

        //     info!("xfer {} is successful.", i);
        // }
        match  sound.pcm_xfer(output_stream_id, &music[..=881000]) {
            Ok(_) => info!("pcm_xfer OK!"),
            Err(_) => error!("pcm_xfer error"),
        }
    }
}
