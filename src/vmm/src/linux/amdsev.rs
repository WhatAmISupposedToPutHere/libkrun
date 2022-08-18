use std::fmt;
use std::fs::File;
use std::io::Read;
use std::mem::{size_of_val, MaybeUninit};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::super::resources::TeeConfig;
use super::vstate::MeasuredRegion;

use codicon::{Decoder, Encoder};
use curl::easy::{Easy, List};
use kbs_types::{Attestation, Challenge, Request, SevChallenge, SevRequest, Tee, TeePubKey};
use kvm_bindings::{kvm_enc_region, kvm_sev_cmd};
use kvm_ioctls::VmFd;
use procfs::CpuInfo;
use serde::{Deserialize, Serialize};
use sev::certs;
use sev::firmware::Firmware;
use sev::launch::sev::{Measurement, Policy, PolicyFlags, Secret, Start};
use sev::session::Session;
use vm_memory::{GuestAddress, GuestMemory, GuestMemoryMmap, GuestMemoryRegion};

#[derive(Debug)]
pub enum Error {
    AttestationRequest(curl::Error),
    DecodeAskArk,
    DecodeCek,
    DecodeChain,
    DownloadCek(curl::Error),
    DownloadAskArk(curl::Error),
    EncodeChain,
    FetchIdentifier,
    InvalidCpuData,
    OpenChainFile(std::io::Error),
    OpenFirmware(std::io::Error),
    OpenTmpFile,
    ParseAttestationSecret(serde_json::Error),
    ParseSevCertConfig(serde_json::Error),
    ParseSessionResponse(serde_json::Error),
    PlatformStatus,
    MemoryEncryptRegion,
    ReadingCpuData(procfs::ProcError),
    ReadingCoreData,
    SessionFromPolicy(std::io::Error),
    SessionRequest(curl::Error),
    SevInit(kvm_ioctls::Error),
    SevInjectSecret(kvm_ioctls::Error),
    SevLaunchFinish(kvm_ioctls::Error),
    SevLaunchMeasure(kvm_ioctls::Error),
    SevLaunchStart(kvm_ioctls::Error),
    SevLaunchUpdateData(kvm_ioctls::Error),
    SevLaunchUpdateVmsa(kvm_ioctls::Error),
    StartFromSession(std::io::Error),
    UnknownCpuModel,
}

struct CurlAgent {
    easy: Easy,
    session_id: Option<String>,
}

fn extract_session_id(header: &[u8]) -> Option<String> {
    let header = match std::str::from_utf8(header) {
        Ok(h) => h,
        Err(_) => return None,
    };

    if !header.contains("session_id") {
        return None;
    }

    let parts: Vec<&str> = header.split(';').collect();
    for p in parts {
        let elems: Vec<&str> = p.split('=').collect();
        if elems.len() == 2 && elems[0].contains("session_id") {
            return Some(elems[1].to_string());
        }
    }

    None
}

impl CurlAgent {
    fn new() -> Self {
        CurlAgent {
            easy: Easy::new(),
            session_id: None,
        }
    }

    fn get(&mut self, url: &str) -> Result<Vec<u8>, curl::Error> {
        let mut rsp = Vec::new();

        self.easy.post(false)?;
        self.easy.url(url)?;

        let mut transfer = self.easy.transfer();
        transfer.write_function(|data| {
            rsp.extend_from_slice(data);
            Ok(data.len())
        })?;
        transfer.perform()?;
        drop(transfer);

        Ok(rsp)
    }

    fn post(&mut self, url: &str, mut data: &[u8]) -> Result<Vec<u8>, curl::Error> {
        let mut rsp = Vec::new();

        let mut headers = List::new();
        headers.append("Accept: application/json")?;
        headers.append("Content-Type: application/json; charset=utf-8")?;
        if let Some(session_id) = &self.session_id {
            headers.append(&format!("Cookie: session_id={}", session_id))?;
        }

        self.easy.post(true)?;
        self.easy.post_field_size(data.len() as u64)?;
        self.easy.url(url)?;
        self.easy.http_headers(headers)?;

        let mut transfer = self.easy.transfer();
        transfer.read_function(|buf| Ok(data.read(buf).unwrap_or(0)))?;
        transfer.write_function(|data| {
            rsp.extend_from_slice(data);
            Ok(data.len())
        })?;
        transfer
            .header_function(|header| {
                if let Some(session_id) = extract_session_id(header) {
                    self.session_id = Some(session_id);
                }
                true
            })
            .unwrap();
        transfer.perform()?;
        drop(transfer);

        Ok(rsp)
    }
}

enum CpuModel {
    Naples,
    Rome,
    Milan,
}

impl fmt::Display for CpuModel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CpuModel::Naples => write!(f, "naples"),
            CpuModel::Rome => write!(f, "rome"),
            CpuModel::Milan => write!(f, "milan"),
        }
    }
}

fn find_cpu_model() -> Result<CpuModel, Error> {
    let cpuinfo = CpuInfo::new().map_err(Error::ReadingCpuData)?;
    let coreinfo = cpuinfo.get_info(0);

    if let Some(coreinfo) = coreinfo {
        match coreinfo.get("cpu family") {
            Some(family) => match *family {
                "23" => match coreinfo.get("model") {
                    Some(model) => match *model {
                        "1" => Ok(CpuModel::Naples),
                        "49" => Ok(CpuModel::Rome),
                        _ => Err(Error::UnknownCpuModel),
                    },
                    None => Err(Error::InvalidCpuData),
                },
                "25" => match coreinfo.get("model") {
                    Some(model) => match *model {
                        "1" => Ok(CpuModel::Milan),
                        _ => Err(Error::UnknownCpuModel),
                    },
                    None => Err(Error::InvalidCpuData),
                },
                _ => Err(Error::UnknownCpuModel),
            },
            None => Err(Error::InvalidCpuData),
        }
    } else {
        Err(Error::ReadingCoreData)
    }
}

fn fetch_chain(fw: &mut Firmware, curl_agent: &mut CurlAgent) -> Result<certs::Chain, Error> {
    const CEK_SVC: &str = "https://kdsintf.amd.com/cek/id";
    const ASK_ARK_SVC: &str = "https://developer.amd.com/wp-content/resources/";

    let mut chain = fw
        .pdh_cert_export()
        .expect("unable to export SEV certificates");

    let id = fw.get_identifier().map_err(|_| Error::FetchIdentifier)?;

    let rsp = curl_agent
        .get(&format!("{}/{}", CEK_SVC, id))
        .map_err(Error::DownloadCek)?;

    chain.cek =
        (certs::sev::Certificate::decode(&mut rsp.as_slice(), ())).map_err(|_| Error::DecodeCek)?;

    let cpu_model = find_cpu_model()?;

    let rsp = curl_agent
        .get(&format!("{}/ask_ark_{}.cert", ASK_ARK_SVC, cpu_model))
        .map_err(Error::DownloadCek)?;

    Ok(certs::Chain {
        ca: certs::ca::Chain::decode(&mut rsp.as_slice(), ()).map_err(|_| Error::DecodeAskArk)?,
        sev: chain,
    })
}

#[derive(Serialize, Deserialize)]
struct SevCertConfig {
    pub vendor_chain: String,
    pub attestation_server_pubkey: String,
}

fn get_and_store_chain(
    fw: &mut Firmware,
    tee_config: &TeeConfig,
    curl_agent: &mut CurlAgent,
) -> Result<certs::Chain, Error> {
    let cert_config: SevCertConfig =
        serde_json::from_str(&tee_config.tee_data).map_err(Error::ParseSevCertConfig)?;

    if !cert_config.vendor_chain.is_empty() {
        let filepath = Path::new(&cert_config.vendor_chain);
        let mut file = File::open(filepath).map_err(Error::OpenChainFile)?;
        Ok(certs::Chain::decode(&mut file, ()).map_err(|_| Error::DecodeChain)?)
    } else {
        let chain = fetch_chain(fw, curl_agent)?;
        let mut file = File::create("/tmp/libkrun-sev.chain").map_err(|_| Error::OpenTmpFile)?;
        chain
            .encode(&mut file, ())
            .map_err(|_| Error::EncodeChain)?;
        Ok(chain)
    }
}

/// Payload sent to the attestation server on session request.
#[derive(Serialize, Deserialize)]
struct SessionRequest {
    build: sev::Build,
    chain: sev::certs::Chain,
}

/// Payload received from the attestation server on session request.
#[derive(Serialize, Deserialize)]
struct SessionResponse {
    id: String,
    start: Start,
}

pub struct AmdSev {
    tee_config: TeeConfig,
    fw: Firmware,
    start: Start,
    session_id: Option<String>,
    sev_es: bool,
    curl_agent: Arc<Mutex<CurlAgent>>,
}

impl AmdSev {
    pub fn new(tee_config: &TeeConfig) -> Result<Self, Error> {
        let mut fw = Firmware::open().map_err(Error::OpenFirmware)?;
        let mut curl_agent = CurlAgent::new();
        let chain = get_and_store_chain(&mut fw, tee_config, &mut curl_agent)?;
        let mut sev_es = false;

        let (start, session_id) = if !tee_config.attestation_url.is_empty() {
            let build = fw
                .platform_status()
                .map_err(|_| Error::PlatformStatus)?
                .build;

            let sev_request = SevRequest { build, chain };
            let request = Request {
                version: "0.0.0".to_string(),
                workload_id: tee_config.workload_id.clone(),
                tee: tee_config.tee.clone(),
                extra_params: serde_json::json!(sev_request).to_string(),
            };

            let response = curl_agent
                .post(
                    format!("{}/kbs/v0/auth", tee_config.attestation_url).as_str(),
                    serde_json::json!(request).to_string().as_bytes(),
                )
                .map_err(Error::SessionRequest)?;

            let challenge: Challenge =
                serde_json::from_slice(&response).map_err(Error::ParseSessionResponse)?;
            let sev_challenge: SevChallenge = serde_json::from_str(&challenge.extra_params)
                .map_err(Error::ParseSessionResponse)?;

            if sev_challenge
                .start
                .policy
                .flags
                .contains(PolicyFlags::ENCRYPTED_STATE)
            {
                sev_es = true;
            }

            (sev_challenge.start, Some(sev_challenge.id))
        } else {
            let policy = Policy::default();
            let session = Session::try_from(policy).map_err(Error::SessionFromPolicy)?;
            (session.start(chain).map_err(Error::StartFromSession)?, None)
        };

        Ok(AmdSev {
            tee_config: tee_config.clone(),
            fw,
            start,
            session_id,
            sev_es,
            curl_agent: Arc::new(Mutex::new(curl_agent)),
        })
    }

    fn sev_init(&self, vm_fd: &VmFd) -> Result<(), kvm_ioctls::Error> {
        let id = if self.sev_es { 1 } else { 0 };

        let mut cmd = kvm_sev_cmd {
            id,
            data: 0,
            error: 0,
            sev_fd: self.fw.as_raw_fd() as u32,
        };

        vm_fd.encrypt_op_sev(&mut cmd)?;
        Ok(())
    }

    fn sev_launch_start(&self, vm_fd: &VmFd) -> Result<(), kvm_ioctls::Error> {
        #[repr(C)]
        struct Data {
            handle: u32,
            policy: Policy,
            dh_addr: u64,
            dh_size: u32,
            session_addr: u64,
            session_size: u32,
        }

        let mut data = Data {
            handle: 0,
            policy: self.start.policy,
            dh_addr: &self.start.cert as *const _ as u64,
            dh_size: size_of_val(&self.start.cert) as u32,
            session_addr: &self.start.session as *const _ as u64,
            session_size: size_of_val(&self.start.session) as u32,
        };

        let mut cmd = kvm_sev_cmd {
            id: 2, // SEV_LAUNCH_START
            data: &mut data as *mut _ as u64,
            error: 0,
            sev_fd: self.fw.as_raw_fd() as u32,
        };

        vm_fd.encrypt_op_sev(&mut cmd)
    }

    fn sev_launch_update_data(
        &self,
        vm_fd: &VmFd,
        data_uaddr: u64,
        data_size: usize,
    ) -> Result<(), kvm_ioctls::Error> {
        #[repr(C)]
        struct Data {
            addr: u64,
            size: u32,
        }

        let mut data = Data {
            addr: data_uaddr as u64,
            size: data_size as u32,
        };

        let mut cmd = kvm_sev_cmd {
            id: 3, // SEV_LAUNCH_UPDATE_DATA
            data: &mut data as *mut _ as u64,
            error: 0,
            sev_fd: self.fw.as_raw_fd() as u32,
        };

        vm_fd.encrypt_op_sev(&mut cmd)
    }

    fn sev_launch_measure(&self, vm_fd: &VmFd) -> Result<Measurement, kvm_ioctls::Error> {
        #[repr(C)]
        struct Data {
            addr: u64,
            size: u32,
        }

        let mut measurement: MaybeUninit<Measurement> = MaybeUninit::uninit();
        let mut data = Data {
            addr: &mut measurement as *mut _ as u64,
            size: size_of_val(&measurement) as u32,
        };

        let mut cmd = kvm_sev_cmd {
            id: 6, // SEV_LAUNCH_MEASURE
            data: &mut data as *mut _ as u64,
            error: 0,
            sev_fd: self.fw.as_raw_fd() as u32,
        };

        vm_fd.encrypt_op_sev(&mut cmd)?;

        Ok(unsafe { measurement.assume_init() })
    }

    fn sev_launch_finish(&self, vm_fd: &VmFd) -> Result<(), kvm_ioctls::Error> {
        let mut cmd = kvm_sev_cmd {
            id: 7, // SEV_LAUNCH_FINISH
            data: 0,
            error: 0,
            sev_fd: self.fw.as_raw_fd() as u32,
        };

        vm_fd.encrypt_op_sev(&mut cmd)
    }

    fn sev_inject_secret(
        &self,
        vm_fd: &VmFd,
        mut secret: Secret,
        secret_host_addr: u64,
    ) -> Result<(), kvm_ioctls::Error> {
        #[repr(C)]
        struct Data {
            headr_addr: u64,
            headr_size: u32,
            guest_addr: u64,
            guest_size: u32,
            trans_addr: u64,
            trans_size: u32,
        }

        let mut data = Data {
            headr_addr: &mut secret.header as *mut _ as u64,
            headr_size: size_of_val(&secret.header) as u32,
            guest_addr: secret_host_addr,
            guest_size: secret.ciphertext.len() as u32,
            trans_addr: secret.ciphertext.as_mut_ptr() as u64,
            trans_size: secret.ciphertext.len() as u32,
        };

        let mut cmd = kvm_sev_cmd {
            id: 5, // SEV_LAUNCH_SECRET
            data: &mut data as *mut _ as u64,
            error: 0,
            sev_fd: vm_fd.as_raw_fd() as u32,
        };

        vm_fd.encrypt_op_sev(&mut cmd)
    }

    fn sev_launch_update_vmsa(&self, vm_fd: &VmFd) -> Result<(), kvm_ioctls::Error> {
        let mut cmd = kvm_sev_cmd {
            id: 4, // SEV_LAUNCH_UPDATE_VMSA
            data: 0,
            error: 0,
            sev_fd: vm_fd.as_raw_fd() as u32,
        };

        vm_fd.encrypt_op_sev(&mut cmd)
    }

    pub fn vm_prepare(&self, vm_fd: &VmFd, guest_mem: &GuestMemoryMmap) -> Result<(), Error> {
        self.sev_init(vm_fd).map_err(Error::SevInit)?;

        for region in guest_mem.iter() {
            // It's safe to unwrap because the guest address is valid.
            let host_addr = guest_mem.get_host_address(region.start_addr()).unwrap();
            let enc_region = kvm_enc_region {
                addr: host_addr as u64,
                size: region.len() as u64,
            };
            vm_fd
                .register_enc_memory_region(&enc_region)
                .map_err(|_| Error::MemoryEncryptRegion)?;
        }

        self.sev_launch_start(vm_fd)
            .map_err(Error::SevLaunchStart)?;

        Ok(())
    }

    pub fn vm_attest(
        &self,
        vm_fd: &VmFd,
        guest_mem: &GuestMemoryMmap,
        measured_regions: Vec<MeasuredRegion>,
    ) -> Result<(), Error> {
        for region in measured_regions {
            self.sev_launch_update_data(vm_fd, region.host_addr, region.size)
                .map_err(Error::SevLaunchUpdateData)?;
        }

        if self.sev_es {
            self.sev_launch_update_vmsa(vm_fd)
                .map_err(Error::SevLaunchUpdateVmsa)?;
        }

        let measurement = self
            .sev_launch_measure(vm_fd)
            .map_err(Error::SevLaunchMeasure)?;

        if !self.tee_config.attestation_url.is_empty() {
            let tee_pubkey = TeePubKey {
                algorithm: "".to_string(),
                pubkey_length: "".to_string(),
                pubkey: "".to_string(),
            };

            let attestation = Attestation {
                nonce: self.session_id.as_ref().unwrap().clone(),
                tee: Tee::Sev,
                tee_pubkey,
                tee_evidence: serde_json::json!(measurement).to_string(),
            };

            let mut curl_agent = self.curl_agent.lock().unwrap();
            curl_agent
                .post(
                    &format!("{}/kbs/v0/attest", self.tee_config.attestation_url,),
                    serde_json::json!(attestation).to_string().as_bytes(),
                )
                .map_err(Error::AttestationRequest)?;

            let secret_resp = curl_agent
                .get(&format!(
                    "{}/kbs/v0/key/{}",
                    self.tee_config.attestation_url, self.tee_config.workload_id,
                ))
                .map_err(Error::AttestationRequest)?;

            let secret: Secret =
                serde_json::from_slice(&secret_resp).map_err(Error::ParseAttestationSecret)?;

            let secret_host_addr = guest_mem
                .get_host_address(GuestAddress(arch::x86_64::layout::CMDLINE_START))
                .unwrap() as u64;
            self.sev_inject_secret(vm_fd, secret, secret_host_addr)
                .map_err(Error::SevInjectSecret)?;
        }

        self.sev_launch_finish(vm_fd)
            .map_err(Error::SevLaunchFinish)?;

        Ok(())
    }
}
