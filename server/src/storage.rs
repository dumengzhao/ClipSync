use crate::models::Network;
use anyhow::Result;
use argon2::password_hash::Error as HashError;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 管理员凭据（**只存哈希，绝不存明文**），落盘到 `<data_dir>/admin.json`。
///
/// 历史上这里存过明文、也整个移除过（那时管理员口令单一来源是环境变量
/// `ADMIN_PASS`，但那样**明文就写在 systemd 的 env 文件里**）。
/// 现在：凭据**只有这一个来源**，且里面只有 Argon2id 哈希。
///
/// 文件不存在 = **尚未初始化**：服务照常启动，但管理 API 全部拒绝，
/// 管理页改为显示「初始化」卡片，由管理员粘贴**自己在别处生成**的哈希串
/// （明文口令从头到尾不经过服务端，环境变量里也不再有它）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminCreds {
    pub user: String,
    /// PHC 字符串：`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>`
    pub pass_hash: String,
    /// 最近一次改密码的时间（Unix 秒）—— 同时作为**会话版本号**：
    /// 改密码后旧 JWT 立即失效（见 `crypto::verify_session` 的 `epoch` 参数）。
    #[serde(default)]
    pub updated_at: u64,
}

/// 文件存储（无数据库）：networks.json / server.key / admin.json
/// 管理员口令的**明文只存在于环境变量或用户脑子里**，落盘与内存里都只有哈希。
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        std::fs::create_dir_all(&dir).ok();
        Store { dir }
    }
    fn networks_path(&self) -> PathBuf {
        self.dir.join("networks.json")
    }
    fn key_path(&self) -> PathBuf {
        self.dir.join("server.key")
    }
    fn admin_path(&self) -> PathBuf {
        self.dir.join("admin.json")
    }

    /// 读取管理员凭据；文件不存在/损坏都返回 `None`（调用方回退到环境变量）。
    pub fn load_admin(&self) -> Option<AdminCreds> {
        let s = std::fs::read_to_string(self.admin_path()).ok()?;
        serde_json::from_str::<AdminCreds>(&s).ok()
    }

    /// 写入管理员凭据（原子写 + 0600）。
    pub fn save_admin(&self, c: &AdminCreds) -> Result<()> {
        let s = serde_json::to_string_pretty(c)?;
        atomic_write(&self.admin_path(), &s)
    }

    /// 改密码：校验旧口令通过后才落新哈希，并刷新 `updated_at`（使旧会话失效）。
    /// 返回新凭据；旧口令不对返回 `Err`。
    pub fn change_password(&self, old_pass: &str, new_pass: &str) -> Result<AdminCreds> {
        let cur = self
            .load_admin()
            .ok_or_else(|| anyhow::anyhow!("凭据未初始化"))?;
        if !verify_pass(&cur.pass_hash, old_pass) {
            anyhow::bail!("当前密码不正确");
        }
        let next = AdminCreds {
            user: cur.user,
            pass_hash: hash_pass(new_pass),
            updated_at: now_unix(),
        };
        self.save_admin(&next)?;
        Ok(next)
    }

    pub fn load_networks(&self) -> Vec<Network> {
        let p = self.networks_path();
        match std::fs::read_to_string(&p) {
            Ok(s) if !s.trim().is_empty() => serde_json::from_str(&s).unwrap_or_default(),
            _ => Vec::new(),
        }
    }
    pub fn save_networks(&self, nets: &[Network]) -> Result<()> {
        let s = serde_json::to_string_pretty(nets)?;
        atomic_write(&self.networks_path(), &s)
    }

    /// 读取会话签名密钥；缺失则随机生成并落盘。
    pub fn load_or_create_key(&self) -> Result<String> {
        let p = self.key_path();
        if let Ok(s) = std::fs::read_to_string(&p) {
            let t = s.trim().to_string();
            if !t.is_empty() {
                return Ok(t);
            }
        }
        let key = crate::crypto::gen_token();
        atomic_write(&p, &key)?;
        Ok(key)
    }
}

/// 原子写：写临时文件 + rename，避免半截文件。
///
/// **落盘即 0600**：这个目录下的东西没有一个是该给别人看的 ——
/// `server.key`（会话签名密钥）、`networks.json`（设备身份）、
/// `github-sync.json`（可能含**代理口令明文**）。默认 0644 会让同机任意用户读到口令。
///
/// 只在 unix 下设置权限：Windows 的 `set_mode` 只映射「只读」属性位，
/// 设 0600 会把文件变成只读，反而让下一次 rename 覆盖失败。
pub fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 口令哈希：**Argon2id**（OWASP 当前推荐的口令哈希算法）。
///
/// 注：argon2 crate 的 `Argon2::default()` 实测**本来就是 Argon2id**（不是 argon2d），
/// 这里显式写出算法与参数只是不再依赖 crate 的默认值 —— 换 crate 版本时行为不会漂移。
/// 参数取 OWASP 的最低档：m=19MiB、t=2、p=1（再往上要看部署机内存）。
///
/// 真正要解决的从来不是「用什么哈希」，而是**明文口令不该出现在任何落盘或配置文件里**：
/// 现在只存哈希（见 `AdminCreds`），env 里已经没有 `ADMIN_PASS` 这个变量了。
pub fn hash_pass(pass: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    let params = Params::new(19 * 1024, 2, 1, None).unwrap_or_default();
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    argon2
        .hash_password(pass.as_bytes(), &salt)
        .expect("argon2 hash")
        .to_string()
}

/// 新口令的最短长度（启动初始化、页面改密码、`hash` 子命令共用同一套规则）
pub const MIN_PASSWORD_LEN: usize = 8;
/// 一律拒绝的默认口令
pub const DEFAULT_PASSWORD: &str = "clipsync";

/// 新口令的强度校验（启动初始化、页面改密码、`hash` 子命令共用同一套规则）。
///
/// 这个口令保护的是「客户端更新包上传」接口——拿到它就能给所有客户端推恶意安装包，
/// 所以太短或仍是默认值一律拒绝，绝不静默放行。
///
/// 注：错误文案是中文，只给管理页（JSON）用；`hash` 子命令走终端、要避开
/// Windows 控制台的 UTF-8 乱码，所以那边自己拼英文短句（规则常量在这里共用）。
pub fn validate_new_password(pass: &str) -> Result<(), String> {
    let p = pass.trim();
    if p.len() < MIN_PASSWORD_LEN {
        return Err(format!("密码至少 {MIN_PASSWORD_LEN} 位"));
    }
    if p == DEFAULT_PASSWORD {
        return Err(format!("密码不能是默认口令 {DEFAULT_PASSWORD}"));
    }
    Ok(())
}

/// 外部哈希串的长度上限。正常 PHC 串不超过约 120 字符，512 足够宽松。
const HASH_INPUT_MAX_LEN: usize = 512;
/// 接受的最大内存参数（KiB）。上限存在的意义见 `check_hash_params_bounded`。
const HASH_MEM_MAX_KIB: u64 = 256 * 1024;
/// 接受的最大迭代次数。
const HASH_TIME_MAX: u64 = 16;
/// 接受的最大并行度。
const HASH_LANES_MAX: u64 = 8;

/// 检查外部哈希串声明的 Argon2 参数是否在合理范围内。
///
/// ⚠️ 这不是「参数调优」，而是**防 DoS**：`verify_password` 会按串里声明的参数
/// 分配内存、跑那么多轮 —— 而初始化端点（未初始化时）是公开的，任何人都能贴一串
/// `m=4194304,t=100,p=8`（4 GiB / 100 轮）进来，让服务端在分配内存时被拖死。
/// 我们自己生成时固定用 19 MiB 档，这里给外部输入留 256 MiB 的宽裕上限即可。
///
/// 直接从串的 params 段（第 4 段 `m=..,t=..,p=..`）解析，不依赖库的内部类型，
/// 解析不出来就不拦（后面的 `verify_password` 会自己失败）。
fn check_hash_params_bounded(s: &str) -> Result<(), String> {
    let seg = s.split('$').nth(3).unwrap_or("");
    for kv in seg.split(',') {
        let Some((k, v)) = kv.split_once('=') else {
            continue;
        };
        let Ok(n) = v.trim().parse::<u64>() else {
            continue;
        };
        match k.trim() {
            "m" if n > HASH_MEM_MAX_KIB => {
                return Err(format!(
                    "哈希串声明的内存参数过大（m={n} KiB，上限 {HASH_MEM_MAX_KIB} KiB）"
                ))
            }
            "t" if n > HASH_TIME_MAX => {
                return Err(format!(
                    "哈希串声明的迭代次数过多（t={n}，上限 {HASH_TIME_MAX}）"
                ))
            }
            "p" if n > HASH_LANES_MAX => {
                return Err(format!(
                    "哈希串声明的并行度过大（p={n}，上限 {HASH_LANES_MAX}）"
                ))
            }
            _ => {}
        }
    }
    Ok(())
}

/// 校验「外部生成的口令哈希串」能不能直接当管理员凭据用。
///
/// 初始化页面允许管理员粘贴自己在别处生成的哈希串 —— 明文口令不经过本服务，
/// 所以这里必须挡住四类坏输入：
/// ① 不是 PHC 串（典型就是把**明文口令**当哈希贴进来）；
/// ② 不是 argon2 家族（bcrypt/scrypt/md5crypt 一律不收）；
/// ③ 串过长、或参数离谱（见 `check_hash_params_bounded` —— 这是防 DoS）；
/// ④ 串本身可解析，但参数不受支持。
///
/// 判据：拿一个**探测口令**去验一次。返回 `Error::Password`（只是口令不匹配）说明
/// 串的格式、算法、参数全都受支持；其它错误一律拒绝。
/// 返回规范化后的哈希串（去掉首尾空白）。
pub fn validate_hash_input(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("哈希串不能为空".into());
    }
    if s.len() > HASH_INPUT_MAX_LEN {
        return Err(format!(
            "哈希串过长（{} 字符，上限 {HASH_INPUT_MAX_LEN}）",
            s.len()
        ));
    }
    let parsed = PasswordHash::new(s).map_err(|_| {
        "不是合法的口令哈希串：应形如 $argon2id$v=19$m=19456,t=2,p=1$<salt>$<hash>，\
         直接贴明文口令是不行的"
            .to_string()
    })?;
    let algo = match parsed.algorithm.as_str() {
        "argon2d" => Algorithm::Argon2d,
        "argon2i" => Algorithm::Argon2i,
        "argon2id" => Algorithm::Argon2id,
        other => {
            return Err(format!(
                "不支持的哈希算法 {other}（只接受 argon2id / argon2i / argon2d）"
            ))
        }
    };
    // 必须在真正跑 Argon2 **之前**拦：这一步就是攻击者想让我们分配内存的地方
    check_hash_params_bounded(s)?;
    match Argon2::new(algo, Version::V0x13, Params::default())
        .verify_password(b"__clipsync_init_probe__", &parsed)
    {
        // 探测口令不可能真的匹配上；真匹配说明这串有问题，拒绝
        Ok(()) => Err("该哈希串校验异常，拒绝使用".into()),
        Err(HashError::Password) => Ok(s.to_string()),
        Err(e) => Err(format!("哈希串不可用：{e}")),
    }
}

/// 校验：算法与参数都**从 PHC 串里取**，这样以后换算法/调参数，老哈希照样能验
/// （硬编码成 Argon2id 实例会让历史哈希全部验不过）。
pub fn verify_pass(hash: &str, pass: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    let algo = match parsed.algorithm.as_str() {
        "argon2d" => Algorithm::Argon2d,
        "argon2i" => Algorithm::Argon2i,
        _ => Algorithm::Argon2id,
    };
    Argon2::new(algo, Version::V0x13, Params::default())
        .verify_password(pass.as_bytes(), &parsed)
        .is_ok()
}

pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_hashed_with_argon2id_and_salted() {
        let h = hash_pass("correct horse battery staple");
        assert!(
            h.starts_with("$argon2id$v=19$"),
            "必须是 Argon2id，实际: {}",
            &h[..h.find('$').unwrap_or(0).max(9)]
        );
        assert!(verify_pass(&h, "correct horse battery staple"));
        assert!(!verify_pass(&h, "wrong password"));
        // 同一个口令两次哈希必须不同（随机盐）
        assert_ne!(h, hash_pass("correct horse battery staple"));
        // 哈希串里绝不能出现口令原文
        assert!(!h.contains("correct horse"));
    }

    #[test]
    fn verify_reads_algorithm_from_phc_string() {
        // 校验必须跟着哈希串里记录的算法走：这样以后换算法/调参数，老哈希照样能验。
        let legacy = Argon2::new(Algorithm::Argon2d, Version::V0x13, Params::default())
            .hash_password(b"legacy-pw", &SaltString::generate(&mut OsRng))
            .unwrap()
            .to_string();
        assert_eq!(legacy.split('$').nth(1), Some("argon2d"));
        assert!(
            verify_pass(&legacy, "legacy-pw"),
            "非 Argon2id 的历史哈希也要能验过"
        );
        assert!(!verify_pass(&legacy, "nope"));
    }

    #[test]
    fn garbage_hash_never_verifies() {
        assert!(!verify_pass("", "x"));
        assert!(!verify_pass("not-a-hash", "x"));
        assert!(!verify_pass("$argon2id$v=19$m=1$", "x"));
    }

    #[test]
    fn validate_hash_input_accepts_only_real_hashes() {
        // 正常串：接受，并返回去掉空白的原文
        let h = hash_pass("some-real-password");
        assert_eq!(validate_hash_input(&h), Ok(h.clone()));
        assert_eq!(
            validate_hash_input(&format!("  {h}\n")),
            Ok(h.clone()),
            "首尾空白要容忍（页面粘贴经常带上换行）"
        );
        // 明文口令：必须挡住（否则「只存哈希」就名存实亡）
        let e = validate_hash_input("my-plain-password").unwrap_err();
        assert!(e.contains("明文"), "错误信息要能提示用户：{e}");
        // 空串 / 乱串
        assert!(validate_hash_input("").is_err());
        assert!(validate_hash_input("   ").is_err());
        assert!(validate_hash_input("$argon2id$v=19$m=1$").is_err());
        assert!(validate_hash_input("not-a-hash").is_err());
    }

    /// 公开的初始化端点不能变成 DoS 入口：外部串声明的 Argon2 参数必须有上限。
    #[test]
    fn oversized_hash_params_are_rejected_before_running_argon2() {
        // m=4 GiB + 100 轮：真跑起来会把服务端拖死 —— 必须在跑之前就拒绝
        let huge = "$argon2id$v=19$m=4194304,t=100,p=8$c2FsdHNhbHRzYWx0c2FsdA$\
                    aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo";
        let e = validate_hash_input(huge).unwrap_err();
        assert!(
            e.contains("过大") || e.contains("过多"),
            "应报参数超限: {e}"
        );

        // 单看每一维也要拦（只超 p 的那种）
        assert!(check_hash_params_bounded("$argon2id$v=19$m=19456,t=2,p=64$x$y").is_err());
        assert!(check_hash_params_bounded("$argon2id$v=19$m=1048576,t=2,p=1$x$y").is_err());
        assert!(check_hash_params_bounded("$argon2id$v=19$m=19456,t=99,p=1$x$y").is_err());

        // 正常档位（含比默认更安全的档）必须放行
        assert!(check_hash_params_bounded("$argon2id$v=19$m=19456,t=2,p=1$x$y").is_ok());
        assert!(check_hash_params_bounded("$argon2id$v=19$m=65536,t=3,p=1$x$y").is_ok());
        // 解析不出参数段时不拦（交给 verify_password 自己失败）
        assert!(check_hash_params_bounded("garbage").is_ok());

        // 超长串直接拒绝，不进解析
        let long = format!("$argon2id$v=19$m=19456,t=2,p=1$xA$yB{}", "z".repeat(600));
        assert!(validate_hash_input(&long).unwrap_err().contains("过长"));
    }
}
