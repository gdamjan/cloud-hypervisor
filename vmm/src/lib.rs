// Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

#[macro_use]
extern crate lazy_static;

#[macro_use]
extern crate log;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate vmm_sys_util;

use crate::api::{ApiError, ApiRequest, ApiResponse, ApiResponsePayload};
use crate::vm::{Error as VmError, ExitBehaviour, Vm};
use libc::EFD_NONBLOCK;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::mpsc::{Receiver, RecvError, SendError, Sender};
use std::sync::Arc;
use std::{result, thread};
use vmm_sys_util::eventfd::EventFd;

pub mod api;
pub mod config;
pub mod device_manager;
pub mod vm;

/// Errors associated with VMM management
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Error {
    /// API request receive error
    ApiRequestRecv(RecvError),

    /// API response receive error
    ApiResponseRecv(RecvError),

    /// API request send error
    ApiRequestSend(SendError<ApiRequest>),

    /// API response send error
    ApiResponseSend(SendError<ApiResponse>),

    /// Cannot create a VM from the API
    ApiVmCreate(ApiError),

    /// Cannot boot a VM from the API
    ApiVmBoot(ApiError),

    /// Cannot shut a VM down from the API
    ApiVmShutdown(ApiError),

    /// Cannot reboot a VM from the API
    ApiVmReboot(ApiError),

    /// Cannot bind to the UNIX domain socket path
    Bind(io::Error),

    /// Cannot clone EventFd.
    EventFdClone(io::Error),

    /// Cannot create EventFd.
    EventFdCreate(io::Error),

    /// Cannot read from EventFd.
    EventFdRead(io::Error),

    /// Cannot write to EventFd.
    EventFdWrite(io::Error),

    /// Cannot create epoll context.
    Epoll(io::Error),

    /// Cannot create HTTP thread
    HttpThreadSpawn(io::Error),

    /// Cannot handle the VM STDIN stream
    Stdin(VmError),

    /// Cannot create a VM
    VmCreate(VmError),

    /// Cannot boot a VM
    VmBoot(VmError),

    /// Cannot shut a VM down
    VmShutdown(VmError),

    /// Cannot create VMM thread
    VmmThreadSpawn(io::Error),
}
pub type Result<T> = result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EpollDispatch {
    Exit,
    Reset,
    Stdin,
    Api,
}

pub struct EpollContext {
    raw_fd: RawFd,
    dispatch_table: Vec<Option<EpollDispatch>>,
}

impl EpollContext {
    pub fn new() -> result::Result<EpollContext, io::Error> {
        let raw_fd = epoll::create(true)?;

        // Initial capacity needs to be large enough to hold:
        // * 1 exit event
        // * 1 reset event
        // * 1 stdin event
        // * 1 API event
        let mut dispatch_table = Vec::with_capacity(5);
        dispatch_table.push(None);

        Ok(EpollContext {
            raw_fd,
            dispatch_table,
        })
    }

    pub fn add_stdin(&mut self) -> result::Result<(), io::Error> {
        let dispatch_index = self.dispatch_table.len() as u64;
        epoll::ctl(
            self.raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            libc::STDIN_FILENO,
            epoll::Event::new(epoll::Events::EPOLLIN, dispatch_index),
        )?;

        self.dispatch_table.push(Some(EpollDispatch::Stdin));

        Ok(())
    }

    fn add_event<T>(&mut self, fd: &T, token: EpollDispatch) -> result::Result<(), io::Error>
    where
        T: AsRawFd,
    {
        let dispatch_index = self.dispatch_table.len() as u64;
        epoll::ctl(
            self.raw_fd,
            epoll::ControlOptions::EPOLL_CTL_ADD,
            fd.as_raw_fd(),
            epoll::Event::new(epoll::Events::EPOLLIN, dispatch_index),
        )?;
        self.dispatch_table.push(Some(token));

        Ok(())
    }
}

impl AsRawFd for EpollContext {
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd
    }
}

pub fn start_vmm_thread(
    http_path: &str,
    api_event: EventFd,
    api_sender: Sender<ApiRequest>,
    api_receiver: Receiver<ApiRequest>,
) -> Result<thread::JoinHandle<Result<()>>> {
    let http_api_event = api_event.try_clone().map_err(Error::EventFdClone)?;

    let thread = thread::Builder::new()
        .name("vmm".to_string())
        .spawn(move || {
            //   let vmm_api_event = api_event.try_clone().map_err(Error::EventFdClone)?;
            let mut vmm = Vmm::new(api_event)?;

            let receiver = Arc::new(api_receiver);
            'outer: loop {
                match vmm.control_loop(Arc::clone(&receiver)) {
                    Ok(ExitBehaviour::Reset) => {
                        // The VMM control loop exites with a reset behaviour.
                        // We have to reboot the VM, i.e. we create a new VM
                        // based on the same VM config, boot it and restart
                        // the control loop.

                        vmm.vm_reboot()?;

                        // Continue and restart the VMM control loop
                        continue 'outer;
                    }
                    Ok(ExitBehaviour::Shutdown) => {
                        // The VMM control loop exites with a shutdown behaviour.
                        // We have to stop the VM and we exit thr thread.
                        if let Some(ref mut vm) = vmm.vm {
                            vm.shutdown().map_err(Error::VmShutdown)?;
                        }
                        break 'outer;
                    }
                    Err(e) => return Err(e),
                }
            }

            Ok(())
        })
        .map_err(Error::VmmThreadSpawn)?;

    // The VMM thread is started, we can start serving HTTP requests
    api::start_http_thread(http_path, http_api_event, api_sender)?;

    Ok(thread)
}

pub struct Vmm {
    epoll: EpollContext,
    exit_evt: EventFd,
    reset_evt: EventFd,
    api_evt: EventFd,
    vm: Option<Vm>,
}

impl Vmm {
    fn new(api_evt: EventFd) -> Result<Self> {
        let mut epoll = EpollContext::new().map_err(Error::Epoll)?;
        let exit_evt = EventFd::new(EFD_NONBLOCK).map_err(Error::EventFdCreate)?;
        let reset_evt = EventFd::new(EFD_NONBLOCK).map_err(Error::EventFdCreate)?;

        if unsafe { libc::isatty(libc::STDIN_FILENO as i32) } != 0 {
            epoll.add_stdin().map_err(Error::Epoll)?;
        }

        epoll
            .add_event(&exit_evt, EpollDispatch::Exit)
            .map_err(Error::Epoll)?;

        epoll
            .add_event(&reset_evt, EpollDispatch::Reset)
            .map_err(Error::Epoll)?;

        epoll
            .add_event(&api_evt, EpollDispatch::Api)
            .map_err(Error::Epoll)?;

        Ok(Vmm {
            epoll,
            exit_evt,
            reset_evt,
            api_evt,
            vm: None,
        })
    }

    fn vm_reboot(&mut self) -> Result<()> {
        // Without ACPI, a reset is equivalent to a shutdown
        #[cfg(not(feature = "acpi"))]
        {
            if let Some(ref mut vm) = self.vm {
                vm.shutdown().map_err(Error::VmShutdown)?;
                return Ok(());
            }
        }

        // First we stop the current VM and create a new one.
        if let Some(ref mut vm) = self.vm {
            let config = vm.get_config();
            vm.shutdown().map_err(Error::VmShutdown)?;

            let exit_evt = self.exit_evt.try_clone().map_err(Error::EventFdClone)?;
            let reset_evt = self.reset_evt.try_clone().map_err(Error::EventFdClone)?;

            self.vm = Some(Vm::new(config, exit_evt, reset_evt).map_err(Error::VmCreate)?);
        }

        // Then we start the new VM.
        if let Some(ref mut vm) = self.vm {
            vm.boot().map_err(Error::VmBoot)?;
        }

        Ok(())
    }

    fn control_loop(&mut self, api_receiver: Arc<Receiver<ApiRequest>>) -> Result<ExitBehaviour> {
        const EPOLL_EVENTS_LEN: usize = 100;

        let mut events = vec![epoll::Event::new(epoll::Events::empty(), 0); EPOLL_EVENTS_LEN];
        let epoll_fd = self.epoll.as_raw_fd();

        let exit_behaviour;

        'outer: loop {
            let num_events = match epoll::wait(epoll_fd, -1, &mut events[..]) {
                Ok(res) => res,
                Err(e) => {
                    if e.kind() == io::ErrorKind::Interrupted {
                        // It's well defined from the epoll_wait() syscall
                        // documentation that the epoll loop can be interrupted
                        // before any of the requested events occurred or the
                        // timeout expired. In both those cases, epoll_wait()
                        // returns an error of type EINTR, but this should not
                        // be considered as a regular error. Instead it is more
                        // appropriate to retry, by calling into epoll_wait().
                        continue;
                    }
                    return Err(Error::Epoll(e));
                }
            };

            for event in events.iter().take(num_events) {
                let dispatch_idx = event.data as usize;

                if let Some(dispatch_type) = self.epoll.dispatch_table[dispatch_idx] {
                    match dispatch_type {
                        EpollDispatch::Exit => {
                            // Consume the event.
                            self.exit_evt.read().map_err(Error::EventFdRead)?;
                            exit_behaviour = ExitBehaviour::Shutdown;

                            break 'outer;
                        }
                        EpollDispatch::Reset => {
                            // Consume the event.
                            self.reset_evt.read().map_err(Error::EventFdRead)?;
                            exit_behaviour = ExitBehaviour::Reset;

                            break 'outer;
                        }
                        EpollDispatch::Stdin => {
                            if let Some(ref vm) = self.vm {
                                vm.handle_stdin().map_err(Error::Stdin)?;
                            }
                        }
                        EpollDispatch::Api => {
                            // Consume the event.
                            self.api_evt.read().map_err(Error::EventFdRead)?;

                            // Read from the API receiver channel
                            let api_request = api_receiver.recv().map_err(Error::ApiRequestRecv)?;

                            match api_request {
                                ApiRequest::VmCreate(config, sender) => {
                                    let exit_evt =
                                        self.exit_evt.try_clone().map_err(Error::EventFdClone)?;
                                    let reset_evt =
                                        self.reset_evt.try_clone().map_err(Error::EventFdClone)?;
                                    let response = match Vm::new(config, exit_evt, reset_evt) {
                                        Ok(vm) => {
                                            self.vm = Some(vm);
                                            Ok(ApiResponsePayload::Empty)
                                        }
                                        Err(e) => Err(ApiError::VmCreate(e)),
                                    };

                                    sender.send(response).map_err(Error::ApiResponseSend)?;
                                }
                                ApiRequest::VmBoot(sender) => {
                                    if let Some(ref mut vm) = self.vm {
                                        let response = match vm.boot() {
                                            Ok(_) => Ok(ApiResponsePayload::Empty),
                                            Err(e) => Err(ApiError::VmBoot(e)),
                                        };

                                        sender.send(response).map_err(Error::ApiResponseSend)?;
                                    }
                                }
                                ApiRequest::VmShutdown(sender) => {
                                    if let Some(ref mut vm) = self.vm {
                                        let response = match vm.shutdown() {
                                            Ok(_) => Ok(ApiResponsePayload::Empty),
                                            Err(e) => Err(ApiError::VmShutdown(e)),
                                        };

                                        sender.send(response).map_err(Error::ApiResponseSend)?;
                                    }
                                }
                                ApiRequest::VmReboot(sender) => {
                                    let response = match self.vm_reboot() {
                                        Ok(_) => Ok(ApiResponsePayload::Empty),
                                        Err(_) => Err(ApiError::VmReboot),
                                    };

                                    sender.send(response).map_err(Error::ApiResponseSend)?;
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(exit_behaviour)
    }
}
