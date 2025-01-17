use board_misoc::csr;
use core::{ptr, slice};
use mailbox;
use rpc_queue;

use kernel_proto::{KERNELCPU_EXEC_ADDRESS, KERNELCPU_LAST_ADDRESS, KSUPPORT_HEADER_SIZE};

#[cfg(has_kernel_cpu)]
pub unsafe fn start() {
    if csr::kernel_cpu::reset_read() == 0 {
        panic!("attempted to start kernel CPU when it is already running")
    }

    stop();

    extern "C" {
        static _binary____ksupport_ksupport_elf_start: u8;
        static _binary____ksupport_ksupport_elf_end: u8;
    }
    let ksupport_elf_start = &_binary____ksupport_ksupport_elf_start as *const u8;
    let ksupport_elf_end = &_binary____ksupport_ksupport_elf_end as *const u8;
    let ksupport_elf = slice::from_raw_parts(
        ksupport_elf_start,
        ksupport_elf_end as usize - ksupport_elf_start as usize,
    );

    if let Err(msg) = load_image(&ksupport_elf) {
        panic!("failed to load kernel CPU image (ksupport.elf): {}", msg);
    }

    csr::kernel_cpu::reset_write(0);

    rpc_queue::init();
}

#[cfg(not(has_kernel_cpu))]
pub unsafe fn start() {
    unimplemented!("not(has_kernel_cpu)")
}

pub unsafe fn stop() {
    #[cfg(has_kernel_cpu)]
    csr::kernel_cpu::reset_write(1);

    mailbox::acknowledge();
    rpc_queue::init();
}

/// Loads the given image for execution on the kernel CPU.
///
/// The entire image including the headers is copied into memory for later use by libunwind, but
/// placed such that the text section ends up at the right location in memory. Currently, we just
/// hard-code the address range, but at least verify that this matches the ELF program header given
/// in the image (avoids loading the – non-relocatable – code at the wrong address on toolchain/…
/// changes).
unsafe fn load_image(image: &[u8]) -> Result<(), &'static str> {
    use dyld::elf::*;
    use dyld::{is_elf_for_current_arch, read_unaligned};

    let ehdr = read_unaligned::<Elf32_Ehdr>(image, 0).map_err(|()| "could not read ELF header")?;

    // The check assumes the two CPUs share the same architecture. This is just to avoid inscrutable
    // errors; we do not functionally rely on this.
    if !is_elf_for_current_arch(&ehdr, ET_EXEC) {
        return Err("not an executable for kernel CPU architecture");
    }

    // First program header should be the main text/… LOAD (see ksupport.ld).
    let phdr = read_unaligned::<Elf32_Phdr>(image, ehdr.e_phoff as usize)
        .map_err(|()| "could not read program header")?;
    if phdr.p_type != PT_LOAD {
        return Err("unexpected program header type");
    }
    if phdr.p_vaddr + phdr.p_memsz > KERNELCPU_LAST_ADDRESS as u32 {
        // This is a weak sanity check only; we also need to fit in the stack, etc.
        return Err("too large for kernel CPU address range");
    }
    const TARGET_ADDRESS: u32 = (KERNELCPU_EXEC_ADDRESS - KSUPPORT_HEADER_SIZE) as _;
    if phdr.p_vaddr - phdr.p_offset != TARGET_ADDRESS {
        return Err("unexpected load address/offset");
    }

    ptr::copy_nonoverlapping(image.as_ptr(), TARGET_ADDRESS as *mut u8, image.len());
    Ok(())
}

pub fn validate(ptr: usize) -> bool {
    ptr >= KERNELCPU_EXEC_ADDRESS && ptr <= KERNELCPU_LAST_ADDRESS
}


#[cfg(has_drtio)]
pub mod subkernel {
    use alloc::{vec::Vec, collections::btree_map::BTreeMap, string::String, string::ToString};
    use core::str;
    use board_artiq::drtio_routing::RoutingTable;
    use board_misoc::clock;
    use proto_artiq::{drtioaux_proto::MASTER_PAYLOAD_MAX_SIZE, rpc_proto as rpc};
    use io::Cursor;
    use rtio_mgt::drtio;
    use sched::{Io, Mutex, Error as SchedError};

    #[derive(Debug, PartialEq, Clone, Copy)]
    pub enum FinishStatus {
        Ok,
        CommLost,
        Exception
    }

    #[derive(Debug, PartialEq, Clone, Copy)]
    pub enum SubkernelState {
        NotLoaded,
        Uploaded,
        Running,
        Finished { status: FinishStatus },
    }

    #[derive(Fail, Debug)]
    pub enum Error {
        #[fail(display = "Timed out waiting for subkernel")]
        Timeout,
        #[fail(display = "Session killed while waiting for subkernel")]
        SessionKilled,
        #[fail(display = "Subkernel is in incorrect state for the given operation")]
        IncorrectState,
        #[fail(display = "DRTIO error: {}", _0)]
        DrtioError(String),
        #[fail(display = "scheduler error")]
        SchedError(SchedError),
        #[fail(display = "rpc io error")]
        RpcIoError,
        #[fail(display = "subkernel finished prematurely")]
        SubkernelFinished,
    }

    impl From<&str> for Error {
        fn from(value: &str) -> Error {
            Error::DrtioError(value.to_string())
        }
    }

    impl From<SchedError> for Error {
        fn from(value: SchedError) -> Error {
            match value {
                SchedError::Interrupted => Error::SessionKilled,
                x => Error::SchedError(x)
            }
        }
    }

    impl From<io::Error<!>> for Error {
        fn from(_value: io::Error<!>) -> Error  {
            Error::RpcIoError
        }
    }

    pub struct SubkernelFinished {
        pub id: u32,
        pub comm_lost: bool,
        pub exception: Option<Vec<u8>>
    }

    struct Subkernel {
        pub destination: u8,
        pub data: Vec<u8>,
        pub state: SubkernelState
    }

    impl Subkernel {
        pub fn new(destination: u8, data: Vec<u8>) -> Self {
            Subkernel {
                destination: destination,
                data: data,
                state: SubkernelState::NotLoaded
            }
        }
    }

    static mut SUBKERNELS: BTreeMap<u32, Subkernel> = BTreeMap::new();

    pub fn add_subkernel(io: &Io, subkernel_mutex: &Mutex, id: u32, destination: u8, kernel: Vec<u8>) {
        let _lock = subkernel_mutex.lock(io).unwrap();
        unsafe { SUBKERNELS.insert(id, Subkernel::new(destination, kernel)); }
    }

    pub fn upload(io: &Io, aux_mutex: &Mutex, subkernel_mutex: &Mutex, 
             routing_table: &RoutingTable, id: u32) -> Result<(), Error> {
        let _lock = subkernel_mutex.lock(io).unwrap();
        let subkernel = unsafe { SUBKERNELS.get_mut(&id).unwrap() };
        drtio::subkernel_upload(io, aux_mutex, routing_table, id, 
            subkernel.destination, &subkernel.data)?;
        subkernel.state = SubkernelState::Uploaded; 
        Ok(()) 
    }

    pub fn load(io: &Io, aux_mutex: &Mutex, subkernel_mutex: &Mutex, routing_table: &RoutingTable,
            id: u32, run: bool) -> Result<(), Error> {
        let _lock = subkernel_mutex.lock(io).unwrap();
        let subkernel = unsafe { SUBKERNELS.get_mut(&id).unwrap() };
        if subkernel.state != SubkernelState::Uploaded {
            return Err(Error::IncorrectState);
        }
        drtio::subkernel_load(io, aux_mutex, routing_table, id, subkernel.destination, run)?;
        if run {
            subkernel.state = SubkernelState::Running;
        }
        Ok(())
    }

    pub fn clear_subkernels(io: &Io, subkernel_mutex: &Mutex) {
        let _lock = subkernel_mutex.lock(io).unwrap();
        unsafe {
            SUBKERNELS = BTreeMap::new();
            MESSAGE_QUEUE = Vec::new();
            CURRENT_MESSAGES = BTreeMap::new();
        }
    }

    pub fn subkernel_finished(io: &Io, subkernel_mutex: &Mutex, id: u32, with_exception: bool) {
        // called upon receiving DRTIO SubkernelRunDone
        let _lock = subkernel_mutex.lock(io).unwrap();
        let subkernel = unsafe { SUBKERNELS.get_mut(&id) };
        // may be None if session ends and is cleared
        if let Some(subkernel) = subkernel {
            subkernel.state = SubkernelState::Finished {
                status: match with_exception {
                true => FinishStatus::Exception,
                false => FinishStatus::Ok,
                }
            }
        }
    }

    pub fn destination_changed(io: &Io, aux_mutex: &Mutex, subkernel_mutex: &Mutex,
             routing_table: &RoutingTable, destination: u8, up: bool) {
        let _lock = subkernel_mutex.lock(io).unwrap();
        let subkernels_iter = unsafe { SUBKERNELS.iter_mut() };
        for (id, subkernel) in subkernels_iter {
            if subkernel.destination == destination {
                if up {
                    match drtio::subkernel_upload(io, aux_mutex, routing_table, *id, destination, &subkernel.data)
                    {
                        Ok(_) => subkernel.state = SubkernelState::Uploaded,
                        Err(e) => error!("Error adding subkernel on destination {}: {}", destination, e)
                    }
                } else {
                    subkernel.state = match subkernel.state {
                        SubkernelState::Running => SubkernelState::Finished { status: FinishStatus::CommLost },
                        _ => SubkernelState::NotLoaded,
                    }
                }
            }
        }
    }

    pub fn retrieve_finish_status(io: &Io, aux_mutex: &Mutex, subkernel_mutex: &Mutex,
        routing_table: &RoutingTable, id: u32) -> Result<SubkernelFinished, Error> {
        let _lock = subkernel_mutex.lock(io)?;
        let mut subkernel = unsafe { SUBKERNELS.get_mut(&id).unwrap() };
        match subkernel.state {
            SubkernelState::Finished { status } => {
                subkernel.state = SubkernelState::Uploaded;
                Ok(SubkernelFinished {
                    id: id,
                    comm_lost: status == FinishStatus::CommLost,
                    exception: if status == FinishStatus::Exception { 
                        Some(drtio::subkernel_retrieve_exception(io, aux_mutex,
                            routing_table, subkernel.destination)?) 
                    } else { None }
                })
            },
            _ => Err(Error::IncorrectState)
        }
    }

    pub fn await_finish(io: &Io, aux_mutex: &Mutex, subkernel_mutex: &Mutex,
        routing_table: &RoutingTable, id: u32, timeout: u64) -> Result<SubkernelFinished, Error> {
        {
            let _lock = subkernel_mutex.lock(io)?;
            match unsafe { SUBKERNELS.get(&id).unwrap().state } {
                SubkernelState::Running | SubkernelState::Finished { .. } => (),
                _ => return Err(Error::IncorrectState)
            }
        }
        let max_time = clock::get_ms() + timeout as u64;
        let _res = io.until(|| {
            if clock::get_ms() > max_time {
                return true;
            }
            if subkernel_mutex.test_lock() {
                // cannot lock again within io.until - scheduler guarantees
                // that it will not be interrupted - so only test the lock
                return false;
            }
            let subkernel = unsafe { SUBKERNELS.get(&id).unwrap() };
            match subkernel.state {
                SubkernelState::Finished { .. } => true,
                _ => false
            }
        })?;
        if clock::get_ms() > max_time {
            error!("Remote subkernel finish await timed out");
            return Err(Error::Timeout);
        }
        retrieve_finish_status(io, aux_mutex, subkernel_mutex, routing_table, id)
    }

    pub struct Message {
        from_id: u32,
        pub tag_count: u8,
        pub tag: u8,
        pub data: Vec<u8>
    }

    // FIFO queue of messages
    static mut MESSAGE_QUEUE: Vec<Message> = Vec::new();
    // currently under construction message(s) (can be from multiple sources)
    static mut CURRENT_MESSAGES: BTreeMap<u32, Message> = BTreeMap::new();

    pub fn message_handle_incoming(io: &Io, subkernel_mutex: &Mutex, 
        id: u32, last: bool, length: usize, data: &[u8; MASTER_PAYLOAD_MAX_SIZE]) {
        // called when receiving a message from satellite
        let _lock = match subkernel_mutex.lock(io) {
            Ok(lock) => lock,
            // may get interrupted, when session is cancelled or main kernel finishes without await
            Err(_) => return,
        };
        if unsafe { SUBKERNELS.get(&id).is_none() } {
            // do not add messages for non-existing or deleted subkernels
            return
        }
        match unsafe { CURRENT_MESSAGES.get_mut(&id) } {
            Some(message) => message.data.extend(&data[..length]),
            None => unsafe {
                CURRENT_MESSAGES.insert(id, Message {
                    from_id: id,
                    tag_count: data[0],
                    tag: data[1],
                    data: data[2..length].to_vec()
                });
            }
        };
        if last {
            unsafe { 
                // when done, remove from working queue
                MESSAGE_QUEUE.push(CURRENT_MESSAGES.remove(&id).unwrap());
            };
        }
    }

    pub fn message_await(io: &Io, subkernel_mutex: &Mutex, id: u32, timeout: u64
    ) -> Result<Message, Error> {
        {
            let _lock = subkernel_mutex.lock(io)?;
            match unsafe { SUBKERNELS.get(&id).unwrap().state } {
                SubkernelState::Finished { .. } => return Err(Error::SubkernelFinished),
                SubkernelState::Running => (),
                _ => return Err(Error::IncorrectState)
            }
        }
        let max_time = clock::get_ms() + timeout as u64;
        let message = io.until_ok(|| {
            if clock::get_ms() > max_time {
                return Ok(None);
            }
            if subkernel_mutex.test_lock() {
                return Err(());
            }
            let msg_len = unsafe { MESSAGE_QUEUE.len() };
            for i in 0..msg_len {
                let msg = unsafe { &MESSAGE_QUEUE[i] };
                if msg.from_id == id {
                    return Ok(Some(unsafe { MESSAGE_QUEUE.remove(i) }));
                }
            }
            match unsafe { SUBKERNELS.get(&id).unwrap().state } {
                SubkernelState::Finished { .. } => return Ok(None),
                _ => ()
            }
            Err(())
        });
        match message {
            Ok(Some(message)) => Ok(message),
            Ok(None) => {
                if clock::get_ms() > max_time {
                    Err(Error::Timeout)
                } else {
                    let _lock = subkernel_mutex.lock(io)?;
                    match unsafe { SUBKERNELS.get(&id).unwrap().state } {
                        SubkernelState::Finished { .. } => Err(Error::SubkernelFinished),
                        _ => Err(Error::IncorrectState)
                    }
                }
            }
            Err(e) => Err(Error::SchedError(e)),
        }
    }

    pub fn message_send<'a>(io: &Io, aux_mutex: &Mutex, subkernel_mutex: &Mutex,
        routing_table: &RoutingTable, id: u32, count: u8, tag: &'a [u8], message: *const *const ()
    ) -> Result<(), Error> {
        let mut writer = Cursor::new(Vec::new());
        let _lock = subkernel_mutex.lock(io).unwrap();
        let destination = unsafe { SUBKERNELS.get(&id).unwrap().destination };

        // reuse rpc code for sending arbitrary data
        rpc::send_args(&mut writer, 0, tag, message)?;
        // skip service tag, but overwrite first byte with tag count
        let data = &mut writer.into_inner()[3..];
        data[0] = count;
        Ok(drtio::subkernel_send_message(
            io, aux_mutex, routing_table, id, destination, data
        )?)
    }
}