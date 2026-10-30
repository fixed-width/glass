//! Local Apple private-desktop negotiation. No framebuffer, input or clipboard operations.
#![forbid(unsafe_code)]

use aes::cipher::{BlockEncrypt, KeyInit};
use crypto_bigint::{
    BoxedUint,
    modular::{BoxedMontyForm, BoxedMontyParams},
};
use md5::{Digest, Md5};
use std::{
    io::{self, Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};
use zeroize::{Zeroize, Zeroizing};

const VERSION: &[u8; 12] = b"RFB 003.889\n";
const APPLE_ACCOUNT: u8 = 30;
const MAX_NAME: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("local login connection failed")]
    Io(#[from] io::Error),
    #[error("local login operation timed out")]
    Timeout,
    #[error("unsupported RFB version")]
    Version,
    #[error("Apple account authentication not offered")]
    AuthenticationUnavailable,
    #[error("invalid or unsupported key-exchange parameters")]
    Parameters,
    #[error("credential field is empty, contains NUL, or exceeds 63 UTF-8 bytes")]
    Credentials,
    #[error("secure random generation failed")]
    Random,
    #[error("account authentication rejected; no automatic retry")]
    Rejected,
    #[error("server initialization is invalid or exceeds bounds")]
    Initialization,
    #[error("private desktop selection is not offered; no console fallback")]
    SelectionUnavailable,
    #[error("private desktop selection message is invalid or unsupported")]
    SessionProtocol,
    #[error("private desktop selection denied; no retry or console fallback")]
    SessionDenied,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Each network phase has an absolute deadline, including partial transfers.
pub struct Wire {
    stream: TcpStream,
    deadline: Instant,
    phase_budget: Duration,
}
impl Wire {
    pub fn new(stream: TcpStream, duration: Duration) -> Result<Self> {
        let deadline = Instant::now().checked_add(duration).ok_or(Error::Timeout)?;
        Ok(Self {
            stream,
            deadline,
            phase_budget: duration,
        })
    }
    fn remaining(&self) -> Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|v| !v.is_zero())
            .ok_or(Error::Timeout)
    }
    fn read<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut bytes = [0; N];
        self.read_into(&mut bytes)?;
        Ok(bytes)
    }
    fn read_into(&mut self, bytes: &mut [u8]) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            self.stream.set_read_timeout(Some(self.remaining()?))?;
            match self.stream.read(&mut bytes[offset..]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
                Ok(n) => offset += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Err(Error::Timeout);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < bytes.len() {
            self.stream.set_write_timeout(Some(self.remaining()?))?;
            match self.stream.write(&bytes[offset..]) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
                Ok(n) => offset += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Err(Error::Timeout);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    pub fn prepare(mut self) -> Result<Prepared> {
        let version = self.read::<12>()?;
        if &version != VERSION {
            return Err(Error::Version);
        }
        self.write(VERSION)?;
        let count = self.read::<1>()?[0] as usize;
        if count == 0 {
            return Err(Error::AuthenticationUnavailable);
        }
        let mut types = vec![0; count];
        self.read_into(&mut types)?;
        if !types.contains(&APPLE_ACCOUNT) {
            return Err(Error::AuthenticationUnavailable);
        }
        self.write(&[APPLE_ACCOUNT])?;
        let generator = u16::from_be_bytes(self.read::<2>()?);
        let length = u16::from_be_bytes(self.read::<2>()?) as usize;
        if !(64..=512).contains(&length) || !length.is_multiple_of(8) {
            return Err(Error::Parameters);
        }
        let mut modulus = vec![0; length];
        let mut peer = vec![0; length];
        self.read_into(&mut modulus)?;
        self.read_into(&mut peer)?;
        let params = Parameters::new(generator, &modulus, &peer)?;
        Ok(Prepared { wire: self, params })
    }
}

struct Exchange {
    public: Box<[u8]>,
    key: Zeroizing<[u8; 16]>,
}

struct Parameters {
    modulus: BoxedUint,
    peer: BoxedUint,
    generator: BoxedUint,
    width: usize,
}
impl Parameters {
    fn new(generator: u16, modulus: &[u8], peer: &[u8]) -> Result<Self> {
        let width = modulus.len();
        if !(64..=512).contains(&width)
            || !width.is_multiple_of(8)
            || peer.len() != width
            || modulus[0] & 0x80 == 0
            || modulus[width - 1] & 1 == 0
            || generator < 2
        {
            return Err(Error::Parameters);
        }
        let bits = (width * 8) as u32;
        let modulus = BoxedUint::from_be_slice(modulus, bits).map_err(|_| Error::Parameters)?;
        let peer = BoxedUint::from_be_slice(peer, bits).map_err(|_| Error::Parameters)?;
        let mut bytes = vec![0; width];
        bytes[width - 2..].copy_from_slice(&generator.to_be_bytes());
        let generator = BoxedUint::from_be_slice(&bytes, bits).map_err(|_| Error::Parameters)?;
        let one = BoxedUint::one_with_precision(bits);
        let high = modulus.wrapping_sub(&one);
        if peer <= one || peer >= high || generator >= high {
            return Err(Error::Parameters);
        }
        Ok(Self {
            modulus,
            peer,
            generator,
            width,
        })
    }
    fn exchange(&self, exponent_bytes: &[u8]) -> Result<Exchange> {
        let exponent = Zeroizing::new(
            BoxedUint::from_be_slice(exponent_bytes, (self.width * 8) as u32)
                .map_err(|_| Error::Parameters)?,
        );
        if *exponent <= BoxedUint::one_with_precision((self.width * 8) as u32) {
            return Err(Error::Parameters);
        }
        let odd = Option::from(self.modulus.to_odd()).ok_or(Error::Parameters)?;
        let params = BoxedMontyParams::new(odd);
        let public = BoxedMontyForm::new(self.generator.clone(), params.clone())
            .pow(&exponent)
            .retrieve()
            .to_be_bytes();
        let shared = Zeroizing::new(BoxedMontyForm::new(self.peer.clone(), params).pow(&exponent));
        let value = Zeroizing::new(shared.retrieve());
        if *value <= BoxedUint::one_with_precision((self.width * 8) as u32) {
            return Err(Error::Parameters);
        }
        let bytes = Zeroizing::new(value.to_be_bytes());
        let mut digest = Md5::digest(&*bytes);
        let mut key = Zeroizing::new([0; 16]);
        key.copy_from_slice(&digest);
        digest.as_mut_slice().zeroize();
        Ok(Exchange { public, key })
    }
}

/// No password or ciphertext is formatted, logged, or returned as a diagnostic.
pub struct Credentials {
    bytes: Zeroizing<[u8; 128]>,
}
impl Credentials {
    pub fn new(username: &[u8], password: &[u8]) -> Result<Self> {
        if [username, password]
            .iter()
            .any(|v| v.is_empty() || v.len() > 63 || v.contains(&0))
        {
            return Err(Error::Credentials);
        }
        let mut bytes = Zeroizing::new([0; 128]);
        getrandom::fill(&mut *bytes).map_err(|_| Error::Random)?;
        bytes[..username.len()].copy_from_slice(username);
        bytes[username.len()] = 0;
        bytes[64..64 + password.len()].copy_from_slice(password);
        bytes[64 + password.len()] = 0;
        Ok(Self { bytes })
    }
}

pub struct Prepared {
    wire: Wire,
    params: Parameters,
}
impl Prepared {
    pub fn local_port(&self) -> Result<u16> {
        Ok(self.wire.stream.local_addr()?.port())
    }

    pub fn modulus_bits(&self) -> usize {
        self.params.width * 8
    }

    /// Caller must bind the installed server/account, obtain local consent and
    /// durably consume dispatch intent before passing credentials to this method.
    /// An error after invocation never authorizes another authentication attempt.
    pub fn authenticate(mut self, credentials: Credentials) -> Result<CredentialsAccepted> {
        // Controller identity verification between phases does not spend the auth budget.
        self.wire.deadline = Instant::now()
            .checked_add(self.wire.phase_budget)
            .ok_or(Error::Timeout)?;
        let mut exponent = Zeroizing::new(vec![0; self.params.width]);
        // A fixed 256-bit random exponent avoids allocation-sized entropy and remains
        // private through constant-time exponentiation at a fixed public precision.
        getrandom::fill(&mut exponent[self.params.width - 32..]).map_err(|_| Error::Random)?;
        exponent[self.params.width - 32] |= 0x80;
        let Exchange { public, key } = self.params.exchange(&exponent)?;
        let cipher = aes::Aes128::new((&*key).into());
        let mut encrypted = Zeroizing::new(*credentials.bytes);
        drop(credentials);
        for block in encrypted.as_chunks_mut::<16>().0 {
            cipher.encrypt_block(block.into());
        }
        self.wire.remaining()?;
        self.wire.write(&*encrypted)?;
        self.wire.write(&public)?;
        drop(encrypted);
        drop(cipher);
        drop(key);
        drop(exponent);
        if u32::from_be_bytes(self.wire.read::<4>()?) != 0 {
            return Err(Error::Rejected);
        }
        Ok(CredentialsAccepted { wire: self.wire })
    }
}

/// Account authentication alone has not selected a private desktop.
pub struct CredentialsAccepted {
    wire: Wire,
}
impl CredentialsAccepted {
    pub fn private_offer(mut self) -> Result<PrivateSessionOffered> {
        self.wire.reset_deadline()?;
        // Shared, extended initialization requests the separate session choice.
        self.wire.write(&[0xc1])?;
        let width = u16::from_be_bytes(self.wire.read::<2>()?);
        let height = u16::from_be_bytes(self.wire.read::<2>()?);
        if width > 8192 || height > 8192 {
            return Err(Error::Initialization);
        }
        let _pixel_format = self.wire.read::<16>()?;
        let length = u32::from_be_bytes(self.wire.read::<4>()?) as usize;
        if length > MAX_NAME {
            return Err(Error::Initialization);
        }
        let mut name = vec![0; length];
        self.wire.read_into(&mut name)?;
        if name.len() < 22 || name[0] != 0 {
            return Err(Error::SelectionUnavailable);
        }
        let flags = u32::from_be_bytes([name[2], name[3], name[4], name[5]]);
        if flags & 4 == 0 || flags & 8 != 0 {
            return Err(Error::SelectionUnavailable);
        }
        let info = self.wire.session_body(11, 74)?;
        if info[..2] != [0, 1] {
            return Err(Error::SessionProtocol);
        }
        let commands = u32::from_be_bytes([info[2], info[3], info[4], info[5]]);
        if commands & (1 << 2) == 0 {
            return Err(Error::SelectionUnavailable);
        }
        let user = &info[10..];
        let Some(end) = user.iter().position(|b| *b == 0) else {
            return Err(Error::SessionProtocol);
        };
        if end == 0 || user[end..].iter().any(|b| *b != 0) || end > 63 {
            return Err(Error::SessionProtocol);
        }
        let mut request = [0; 74];
        request[..4].copy_from_slice(&[0, 72, 0, 1]);
        request[8] = 2;
        request[10..10 + end].copy_from_slice(&user[..end]);
        Ok(PrivateSessionOffered {
            wire: self.wire,
            request,
        })
    }
}

impl Wire {
    fn reset_deadline(&mut self) -> Result<()> {
        self.deadline = Instant::now()
            .checked_add(self.phase_budget)
            .ok_or(Error::Timeout)?;
        Ok(())
    }

    fn session_body(&mut self, minimum: usize, maximum: usize) -> Result<Vec<u8>> {
        let length = u16::from_be_bytes(self.read::<2>()?) as usize;
        if !(minimum..=maximum).contains(&length) {
            return Err(Error::SessionProtocol);
        }
        let mut body = vec![0; length];
        self.read_into(&mut body)?;
        Ok(body)
    }
}

/// The server offered a private desktop; no selection command has been sent yet.
pub struct PrivateSessionOffered {
    wire: Wire,
    request: [u8; 74],
}
impl PrivateSessionOffered {
    pub fn select_private(mut self) -> Result<PrivateSessionGranted> {
        self.wire.reset_deadline()?;
        self.wire.write(&self.request)?;
        for _ in 0..16 {
            let result = self.wire.session_body(6, 80)?;
            if result[..2] != [0, 1] {
                return Err(Error::SessionProtocol);
            }
            let status = u32::from_be_bytes([result[2], result[3], result[4], result[5]]);
            match status {
                0 | 4 => return Ok(PrivateSessionGranted { _wire: self.wire }),
                2 | 3 => {}
                _ => return Err(Error::SessionDenied),
            }
        }
        Err(Error::SessionProtocol)
    }
}

/// Retain until the actual signed worker proves private graphical readiness.
/// Drop closes the transport. This type exposes no frames, input or clipboard.
pub struct PrivateSessionGranted {
    _wire: Wire,
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockDecrypt;
    use std::{net::TcpListener, sync::mpsc, thread};

    fn prime() -> Vec<u8> {
        let hex = concat!(
            "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
            "29024E088A67CC74020BBEA63B139B22514A08798E3404DDEF",
            "9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E4",
            "85B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE",
            "386BFB5A899FA5AE9F24117C4B1FE649286651ECE65381FFF",
            "FFFFFFFFFFFFF"
        );
        hex.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| u8::from_str_radix(std::str::from_utf8(v).unwrap(), 16).unwrap())
            .collect()
    }
    fn small(n: u32, width: usize) -> Vec<u8> {
        let mut bytes = vec![0; width];
        bytes[width - 4..].copy_from_slice(&n.to_be_bytes());
        bytes
    }
    fn scripted<T: Send + 'static>(
        duration: Duration,
        body: impl FnOnce(TcpStream) -> T + Send + 'static,
    ) -> (Wire, thread::JoinHandle<T>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let job = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            body(stream)
        });
        (Wire::new(stream, duration).unwrap(), job)
    }
    use std::net::Ipv4Addr;
    fn read<const N: usize>(stream: &mut TcpStream) -> [u8; N] {
        let mut bytes = [0; N];
        stream.read_exact(&mut bytes).unwrap();
        bytes
    }
    fn offer(stream: &mut TcpStream) {
        stream.write_all(b"RFB 003.889\n").unwrap();
        assert_eq!(&read::<12>(stream), VERSION);
        stream.write_all(&[3, 1, 2, 30]).unwrap();
        assert_eq!(read::<1>(stream), [30]);
        let p = prime();
        assert_eq!(p.len(), 128);
        stream.write_all(&2_u16.to_be_bytes()).unwrap();
        stream.write_all(&(p.len() as u16).to_be_bytes()).unwrap();
        stream.write_all(&p).unwrap();
        stream.write_all(&small(1 << 17, p.len())).unwrap();
    }
    fn answer(stream: &mut TcpStream) {
        let mut encrypted = read::<128>(stream);
        let public = read::<128>(stream);
        let params = Parameters::new(2, &prime(), &public).unwrap();
        let Exchange { key, .. } = params.exchange(&small(17, 128)).unwrap();
        let cipher = aes::Aes128::new((&*key).into());
        for block in encrypted.as_chunks_mut::<16>().0 {
            cipher.decrypt_block(block.into());
        }
        assert_eq!(&encrypted[..6], b"agent\0");
        assert_eq!(&encrypted[64..80], b"test-only-dummy\0");
        encrypted.zeroize();
    }
    fn initialize(stream: &mut TcpStream, name_length: u32) {
        stream.write_all(&0_u32.to_be_bytes()).unwrap();
        assert_eq!(read::<1>(stream), [0xc1]);
        stream.write_all(&1280_u16.to_be_bytes()).unwrap();
        stream.write_all(&900_u16.to_be_bytes()).unwrap();
        stream.write_all(&[0; 16]).unwrap();
        stream.write_all(&name_length.to_be_bytes()).unwrap();
    }
    fn offer_selection(stream: &mut TcpStream) {
        let mut name = vec![0; 25];
        name[5] = 4;
        name[22..].copy_from_slice(b"dev");
        stream.write_all(&name).unwrap();
        let mut info = [0; 74];
        info[1] = 1;
        info[5] = 4;
        info[10..13].copy_from_slice(b"mpd");
        stream.write_all(&74_u16.to_be_bytes()).unwrap();
        stream.write_all(&info).unwrap();
    }
    fn grant_selection(stream: &mut TcpStream) {
        let mut expected = [0; 74];
        expected[..4].copy_from_slice(&[0, 72, 0, 1]);
        expected[8] = 2;
        expected[10..13].copy_from_slice(b"mpd");
        assert_eq!(read::<74>(stream), expected);
        stream.write_all(&80_u16.to_be_bytes()).unwrap();
        stream.write_all(&[0, 1, 0, 0, 0, 0]).unwrap();
        stream.write_all(&[0; 74]).unwrap();
    }
    fn private_login(wire: Wire) -> Result<PrivateSessionGranted> {
        wire.prepare()?
            .authenticate(Credentials::new(b"agent", b"test-only-dummy")?)?
            .private_offer()?
            .select_private()
    }
    fn extended_name(flags: u8) -> Vec<u8> {
        let mut name = vec![0; 22];
        name[5] = flags;
        name
    }
    fn selection_intro(stream: &mut TcpStream, name: &[u8]) {
        assert_eq!(read::<1>(stream), [0xc1]);
        stream.write_all(&[0; 20]).unwrap();
        stream
            .write_all(&(name.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(name).unwrap();
    }
    fn selection_info(stream: &mut TcpStream, version: u16, commands: u32, user: &[u8]) {
        stream
            .write_all(&((10 + user.len()) as u16).to_be_bytes())
            .unwrap();
        stream.write_all(&version.to_be_bytes()).unwrap();
        stream.write_all(&commands.to_be_bytes()).unwrap();
        stream.write_all(&[0; 4]).unwrap();
        stream.write_all(user).unwrap();
    }
    fn assert_eof(stream: &mut TcpStream) {
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    }
    fn offered_script<T: Send + 'static>(
        body: impl FnOnce(TcpStream) -> T + Send + 'static,
    ) -> (PrivateSessionOffered, thread::JoinHandle<T>) {
        let (wire, job) = scripted(Duration::from_secs(2), move |mut stream| {
            selection_intro(&mut stream, &extended_name(4));
            selection_info(&mut stream, 1, 4, b"mpd\0");
            assert_eq!(read::<74>(&mut stream)[8], 2);
            body(stream)
        });
        let offered = CredentialsAccepted { wire }.private_offer().unwrap();
        (offered, job)
    }
    fn selection_result(stream: &mut TcpStream, status: u32) {
        stream.write_all(&80_u16.to_be_bytes()).unwrap();
        stream.write_all(&1_u16.to_be_bytes()).unwrap();
        stream.write_all(&status.to_be_bytes()).unwrap();
        stream.write_all(&[0; 74]).unwrap();
    }
    #[test]
    fn refuses_standard_version_without_downgrade_or_authentication() {
        for version in [b"RFB 003.008\n", b"RFB 003.009\n", b"RFB 004.889\n"] {
            let (wire, server) = scripted(Duration::from_secs(2), move |mut stream| {
                stream.write_all(version).unwrap();
                assert_eof(&mut stream);
            });
            assert!(matches!(wire.prepare(), Err(Error::Version)));
            server.join().unwrap();
        }
    }
    #[test]
    fn no_client_initialization_after_authentication_until_explicitly_requested() {
        let (wire, server) = scripted(Duration::from_secs(2), |mut stream| {
            offer(&mut stream);
            answer(&mut stream);
            stream.write_all(&0_u32.to_be_bytes()).unwrap();
            assert_eof(&mut stream);
        });
        drop(
            wire.prepare()
                .unwrap()
                .authenticate(Credentials::new(b"agent", b"test-only-dummy").unwrap())
                .unwrap(),
        );
        server.join().unwrap();
    }
    #[test]
    fn refuses_absent_selection_and_no_virtual_desktop_flags_without_fallback() {
        for name in [
            b"ordinary desktop".to_vec(),
            extended_name(0),
            extended_name(12),
        ] {
            let (wire, server) = scripted(Duration::from_secs(2), move |mut stream| {
                selection_intro(&mut stream, &name);
                assert_eof(&mut stream);
            });
            assert!(matches!(
                CredentialsAccepted { wire }.private_offer(),
                Err(Error::SelectionUnavailable)
            ));
            server.join().unwrap();
        }
    }
    #[test]
    fn refuses_console_only_commands_without_sending_a_selection() {
        let (wire, server) = scripted(Duration::from_secs(2), |mut stream| {
            selection_intro(&mut stream, &extended_name(4));
            selection_info(&mut stream, 1, 3, b"mpd\0");
            assert_eof(&mut stream);
        });
        assert!(matches!(
            CredentialsAccepted { wire }.private_offer(),
            Err(Error::SelectionUnavailable)
        ));
        server.join().unwrap();
    }
    #[test]
    fn uses_the_servers_routing_name_only_for_the_private_command() {
        let (wire, server) = scripted(Duration::from_secs(2), |mut stream| {
            selection_intro(&mut stream, &extended_name(4));
            selection_info(&mut stream, 1, 4, b"other\0");
            let request = read::<74>(&mut stream);
            assert_eq!(request[8], 2);
            assert_eq!(&request[10..16], b"other\0");
            selection_result(&mut stream, 0);
            assert_eof(&mut stream);
        });
        drop(
            CredentialsAccepted { wire }
                .private_offer()
                .unwrap()
                .select_private()
                .unwrap(),
        );
        server.join().unwrap();
    }
    #[test]
    fn refuses_malformed_or_unsupported_selection_info() {
        for (version, user) in [
            (0, b"mpd\0".as_slice()),
            (2, b"mpd\0"),
            (1, b"mpd"),
            (1, b"mpd\0other"),
        ] {
            let (wire, server) = scripted(Duration::from_secs(2), move |mut stream| {
                selection_intro(&mut stream, &extended_name(4));
                selection_info(&mut stream, version, 4, user);
                assert_eof(&mut stream);
            });
            assert!(matches!(
                CredentialsAccepted { wire }.private_offer(),
                Err(Error::SessionProtocol)
            ));
            server.join().unwrap();
        }
    }
    #[test]
    fn refuses_selection_info_length_before_waiting_for_its_payload() {
        for length in [0_u16, 10, 75, u16::MAX] {
            let (wire, server) = scripted(Duration::from_secs(2), move |mut stream| {
                selection_intro(&mut stream, &extended_name(4));
                stream.write_all(&length.to_be_bytes()).unwrap();
                assert_eof(&mut stream);
            });
            assert!(matches!(
                CredentialsAccepted { wire }.private_offer(),
                Err(Error::SessionProtocol)
            ));
            server.join().unwrap();
        }
    }
    #[test]
    fn dropping_an_offer_sends_no_selection_command() {
        let (wire, server) = scripted(Duration::from_secs(2), |mut stream| {
            selection_intro(&mut stream, &extended_name(4));
            selection_info(&mut stream, 1, 4, b"mpd\0");
            assert_eof(&mut stream);
        });
        drop(CredentialsAccepted { wire }.private_offer().unwrap());
        server.join().unwrap();
    }
    #[test]
    fn a_denied_private_session_sends_no_retry_or_console_fallback() {
        for status in [1, 5, u32::MAX] {
            let (offered, server) = offered_script(move |mut stream| {
                selection_result(&mut stream, status);
                assert_eof(&mut stream);
            });
            assert!(matches!(
                offered.select_private(),
                Err(Error::SessionDenied)
            ));
            server.join().unwrap();
        }
    }
    #[test]
    fn pending_private_session_can_be_granted_without_repeating_the_command() {
        let (offered, server) = offered_script(|mut stream| {
            for status in [2, 3, 4] {
                selection_result(&mut stream, status);
            }
            assert_eof(&mut stream);
        });
        drop(offered.select_private().unwrap());
        server.join().unwrap();
    }
    #[test]
    fn refuses_an_unbounded_pending_result_stream() {
        let (offered, server) = offered_script(|mut stream| {
            for _ in 0..16 {
                selection_result(&mut stream, 2);
            }
            assert_eof(&mut stream);
        });
        assert!(matches!(
            offered.select_private(),
            Err(Error::SessionProtocol)
        ));
        server.join().unwrap();
    }
    #[test]
    fn refuses_malformed_result_lengths_and_versions() {
        for (length, version) in [(0_u16, 1_u16), (5, 1), (81, 1), (u16::MAX, 1), (80, 2)] {
            let (offered, server) = offered_script(move |mut stream| {
                stream.write_all(&length.to_be_bytes()).unwrap();
                if length == 80 {
                    stream.write_all(&version.to_be_bytes()).unwrap();
                    stream.write_all(&[0; 78]).unwrap();
                }
                assert_eof(&mut stream);
            });
            assert!(matches!(
                offered.select_private(),
                Err(Error::SessionProtocol)
            ));
            server.join().unwrap();
        }
    }
    #[test]
    fn partial_selection_progress_cannot_extend_its_deadline() {
        let (wire, server) = scripted(Duration::from_millis(60), |mut stream| {
            assert_eq!(read::<74>(&mut stream)[8], 2);
            for byte in [0, 80, 0, 1, 0, 0, 0, 2] {
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        let mut request = [0; 74];
        request[8] = 2;
        assert!(matches!(
            PrivateSessionOffered { wire, request }.select_private(),
            Err(Error::Timeout)
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
    }
    #[test]
    fn observation_between_selection_phases_does_not_spend_the_network_deadline() {
        let (wire, server) = scripted(Duration::from_millis(300), |mut stream| {
            selection_intro(&mut stream, &extended_name(4));
            selection_info(&mut stream, 1, 4, b"mpd\0");
            assert_eq!(read::<74>(&mut stream)[8], 2);
            selection_result(&mut stream, 0);
            assert_eof(&mut stream);
        });
        thread::sleep(Duration::from_millis(400));
        let offered = CredentialsAccepted { wire }.private_offer().unwrap();
        thread::sleep(Duration::from_millis(400));
        drop(offered.select_private().unwrap());
        server.join().unwrap();
    }
    #[test]
    fn aes_matches_nist_zero_block_known_answer() {
        let cipher = aes::Aes128::new((&[0; 16]).into());
        let mut block = [0; 16];
        cipher.encrypt_block((&mut block).into());
        assert_eq!(
            block,
            [
                0x66, 0xe9, 0x4b, 0xd4, 0xef, 0x8a, 0x2c, 0x3b, 0x88, 0x4c, 0xfa, 0x59, 0xca, 0x34,
                0x2b, 0x2e
            ]
        );
    }
    #[test]
    fn exchange_matches_independent_fixed_width_big_endian_vector() {
        let params = Parameters::new(2, &prime(), &small(1 << 17, 128)).unwrap();
        let Exchange { public, key } = params.exchange(&small(13, 128)).unwrap();
        assert_eq!(&*public, &small(1 << 13, 128));
        assert_eq!(
            *key,
            [
                0xe9, 0x21, 0xea, 0xa5, 0x8f, 0x31, 0x90, 0x97, 0x0c, 0x5f, 0x9a, 0xa9, 0xd3, 0xbe,
                0xa6, 0xf8
            ]
        );
    }
    #[test]
    fn rejects_truncated_or_ambiguous_credential_fields() {
        for (user, password) in [
            (b"".as_slice(), b"x".as_slice()),
            (b"agent", b""),
            (b"a\0b", b"x"),
            (b"agent", b"a\0b"),
            (b"agent", &[b'x'; 64]),
            (&[b'x'; 64], b"x"),
        ] {
            assert!(matches!(
                Credentials::new(user, password),
                Err(Error::Credentials)
            ));
        }
        assert!(Credentials::new(&[b'x'; 63], &[b'y'; 63]).is_ok());
    }

    #[test]
    fn exchange_preserves_padding_at_the_actual_4096_bit_server_width() {
        // This vector checks maximum precision and encoding, not prime validation.
        let params = Parameters::new(2, &[255; 512], &small(1 << 17, 512)).unwrap();
        let Exchange { public, key } = params.exchange(&small(13, 512)).unwrap();
        assert_eq!(&*public, &small(1 << 13, 512));
        assert_eq!(
            *key,
            [
                0x50, 0x3c, 0xe8, 0x9a, 0x12, 0xf3, 0x57, 0xd4, 0x3c, 0xc4, 0x5d, 0x9a, 0x3b, 0x46,
                0x57, 0xd7
            ]
        );
    }
    #[test]
    fn rejects_even_or_short_modulus_and_trivial_peer_values() {
        let p = prime();
        let mut even = p.clone();
        even[127] &= 0xfe;
        let mut high = p.clone();
        high[127] -= 1;
        assert!(matches!(
            Parameters::new(2, &even, &small(2, 128)),
            Err(Error::Parameters)
        ));
        assert!(matches!(
            Parameters::new(2, &[255; 8], &[2; 8]),
            Err(Error::Parameters)
        ));
        for peer in [small(0, 128), small(1, 128), high, p.clone()] {
            assert!(matches!(
                Parameters::new(2, &p, &peer),
                Err(Error::Parameters)
            ));
        }
    }
    #[test]
    fn preflight_stops_before_credentials_or_client_initialization() {
        let (wire, server) = scripted(Duration::from_secs(2), |mut stream| {
            offer(&mut stream);
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        let prepared = wire.prepare().unwrap();
        assert_eq!(prepared.modulus_bits(), 1024);
        drop(prepared);
        server.join().unwrap();
    }
    #[test]
    fn refuses_missing_apple_authentication_without_falling_back() {
        let (wire, server) = scripted(Duration::from_secs(2), |mut stream| {
            stream.write_all(VERSION).unwrap();
            read::<12>(&mut stream);
            stream.write_all(&[2, 1, 2]).unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        assert!(matches!(
            wire.prepare(),
            Err(Error::AuthenticationUnavailable)
        ));
        server.join().unwrap();
    }
    #[test]
    fn refuses_oversized_exchange_before_waiting_for_payload() {
        let (wire, server) = scripted(Duration::from_secs(2), |mut stream| {
            stream.write_all(VERSION).unwrap();
            read::<12>(&mut stream);
            stream.write_all(&[1, 30]).unwrap();
            read::<1>(&mut stream);
            stream.write_all(&[0, 2, 255, 255]).unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        assert!(matches!(wire.prepare(), Err(Error::Parameters)));
        server.join().unwrap();
    }
    #[test]
    fn partial_progress_cannot_extend_the_absolute_deadline() {
        let (wire, server) = scripted(Duration::from_millis(60), |mut stream| {
            for b in VERSION {
                if stream.write_all(&[*b]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        assert!(matches!(wire.prepare(), Err(Error::Timeout)));
        assert!(start.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
    }
    #[test]
    fn controller_verification_delay_does_not_spend_the_authentication_deadline() {
        let (wire, server) = scripted(Duration::from_millis(500), |mut stream| {
            offer(&mut stream);
            answer(&mut stream);
            initialize(&mut stream, 25);
            offer_selection(&mut stream);
            grant_selection(&mut stream);
        });
        let prepared = wire.prepare().unwrap();
        thread::sleep(Duration::from_millis(600));
        let held = prepared
            .authenticate(Credentials::new(b"agent", b"test-only-dummy").unwrap())
            .unwrap()
            .private_offer()
            .unwrap()
            .select_private()
            .unwrap();
        drop(held);
        server.join().unwrap();
    }
    #[test]
    fn granted_private_connection_stays_open_without_frames_input_or_clipboard() {
        let (send, receive) = mpsc::channel();
        let (wire, server) = scripted(Duration::from_secs(2), move |mut stream| {
            offer(&mut stream);
            answer(&mut stream);
            initialize(&mut stream, 25);
            offer_selection(&mut stream);
            grant_selection(&mut stream);
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut byte = [0];
            let error = stream.read(&mut byte).unwrap_err();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ));
            send.send(()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        let held = private_login(wire).unwrap();
        receive.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(held);
        server.join().unwrap();
    }
    #[test]
    fn rejected_authentication_sends_no_retry_or_initialization() {
        let (wire, server) = scripted(Duration::from_secs(2), |mut stream| {
            offer(&mut stream);
            answer(&mut stream);
            stream.write_all(&1_u32.to_be_bytes()).unwrap();
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        assert!(matches!(
            wire.prepare()
                .unwrap()
                .authenticate(Credentials::new(b"agent", b"test-only-dummy").unwrap()),
            Err(Error::Rejected)
        ));
        server.join().unwrap();
    }
    #[test]
    fn refuses_oversized_server_name_without_reading_or_logging_it() {
        let (wire, server) = scripted(Duration::from_secs(2), |mut stream| {
            offer(&mut stream);
            answer(&mut stream);
            initialize(&mut stream, (MAX_NAME + 1) as u32);
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        });
        assert!(matches!(
            wire.prepare()
                .unwrap()
                .authenticate(Credentials::new(b"agent", b"test-only-dummy").unwrap())
                .unwrap()
                .private_offer(),
            Err(Error::Initialization)
        ));
        server.join().unwrap();
    }
}
