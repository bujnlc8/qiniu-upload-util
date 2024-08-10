//! 七牛文件上传工具

use clap::{CommandFactory, Parser};
use clap_complete::{generate, Shell};
use colored::Colorize;
use qiniu_uploader::{QiniuRegionEnum, QiniuUploader};
use qrcode::{render::unicode, QrCode};
use std::{
    io::{self},
    os::unix::fs::MetadataExt,
    str::FromStr,
    time,
};
use std::{path::PathBuf, process::exit};
use tokio::{
    fs::{self, File},
    io::AsyncRead,
};

// 将列表分块
fn split_into_chunks<T>(list: Vec<T>, chunk_count: usize) -> Vec<Vec<T>>
where
    T: Clone,
{
    let chunk_size = (list.len() + chunk_count - 1) / chunk_count;
    list.chunks(chunk_size)
        .map(|chunk| chunk.to_vec())
        .collect()
}

// 获取下载链接
fn get_download_url(domain_name: Option<String>, object_name: &str) -> String {
    match domain_name {
        Some(domain_name) => {
            if domain_name.starts_with("http") {
                format!("{domain_name}/{object_name}")
            } else {
                format!("https://{domain_name}/{object_name}")
            }
        }
        None => "".to_string(),
    }
}

// 遍历目录
fn walk_dir(dir: PathBuf) -> Vec<PathBuf> {
    let mut res = Vec::new();
    if dir.is_dir() {
        for item in dir.read_dir().unwrap() {
            let item_path = item.unwrap().path();
            if item_path.is_file() {
                res.push(item_path);
            } else {
                res.extend(walk_dir(item_path));
            }
        }
    } else {
        res.push(dir);
    }
    res
}

/// 上传文件到七牛，开启进度条
pub async fn upload_to_qiniu<R: AsyncRead + Send + Sync + 'static + std::marker::Unpin>(
    qiniu: QiniuUploader,
    reader: R,
    object_name: &str,
    file_size: usize,
    part_size: Option<usize>,
    threads: Option<u8>,
) -> Result<(), anyhow::Error> {
    #[cfg(feature = "progress-bar")]
    qiniu
        .part_upload_file(object_name, reader, file_size, part_size, threads, None)
        .await?;

    #[cfg(not(feature = "progress-bar"))]
    qiniu
        .part_upload_file(object_name, reader, file_size, part_size, threads)
        .await?;
    Ok(())
}

#[derive(Parser)]
#[clap(version, about, long_about=None)]
pub struct Cli {
    /// 七牛access key，或自动从环境变量 `QINIU_ACCESS_KEY` 获取
    #[clap(short, long)]
    access_key: Option<String>,
    /// 七牛secret key, 或自动从环境变量 `QINIU_SECRET_KEY` 获取
    #[clap(short, long)]
    secret_key: Option<String>,
    /// 对象名称，如果未指定会从`file_path`参数解析，一般不建议设置
    #[clap(short, long)]
    object_name: Option<String>,
    /// 文件绝对路径，支持目录
    #[clap(short, long)]
    file_path: Option<PathBuf>,
    /// 七牛bucket名称
    #[clap(short, long)]
    bucket_name: Option<String>,
    /// 七牛bucket region，如z0，华东-浙江(默认)，详见 https://developer.qiniu.com/kodo/1671/region-endpoint-fq
    #[clap(long)]
    region: Option<String>,
    /// 下载域名，需要和bucket匹配，如果设置，会显示下载链接及输出二维码
    #[clap(short, long)]
    domain_name: Option<String>,
    /// 生成shell补全脚本, 支持Bash, Zsh, Fish, PowerShell, Elvish
    #[arg(long)]
    completion: Option<String>,
    /// 不要输出下载链接二维码
    #[clap(long, action)]
    no_qrcode: bool,
    /// 分片上传的大小，单位bytes，1M-1GB之间，如果指定，优先级比threads参数高
    #[arg(long)]
    part_size: Option<usize>,
    /// 分片上传线程，在未指定part_size参数的情况下生效，默认5
    #[arg(long)]
    threads: Option<u8>,
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::parse();
    let start = time::Instant::now();
    if let Some(shell) = cli.completion {
        let mut cmd = Cli::command();
        let bin_name = cmd.get_name().to_string();
        match Shell::from_str(&shell.to_lowercase()) {
            Ok(shell) => generate(shell, &mut cmd, bin_name, &mut io::stdout()),
            Err(e) => {
                eprintln!("{}", e.to_string().red());
                exit(1)
            }
        };
        return Ok(());
    }
    let qiniu_access_key = match cli.access_key {
        Some(key) => key,
        None => match std::env::var("QINIU_ACCESS_KEY") {
            Ok(key) => key,
            Err(_) => {
                eprintln!("{}", "Qiniu access_key 为空！".red());
                exit(1)
            }
        },
    };
    let qiniu_secret_key = match cli.secret_key {
        Some(key) => key,
        None => match std::env::var("QINIU_SECRET_KEY") {
            Ok(key) => key,
            Err(_) => {
                eprintln!("{}", "Qiniu secret_key 为空！".red());
                exit(1)
            }
        },
    };
    let file_path = cli.file_path.unwrap_or_else(|| {
        eprintln!("{}", "file-path is required !".red());
        exit(1);
    });
    let bucket_name = cli.bucket_name.unwrap_or_else(|| {
        eprintln!("{}", "bucket-name is required !".red());
        exit(1);
    });
    let region = QiniuRegionEnum::from_str(&cli.region.unwrap_or("z0".to_string())).unwrap();
    let qiniu = QiniuUploader::new(
        qiniu_access_key.clone(),
        qiniu_secret_key.clone(),
        bucket_name,
        Some(region),
        false,
    );
    // 上传目录
    if file_path.is_dir() {
        let item_path = walk_dir(file_path.clone());
        if item_path.is_empty() {
            eprintln!("{}", "空文件夹！".red());
            exit(1);
        }
        // 最多30个线程上传
        let item_path_list = split_into_chunks(item_path, 30);
        let dir_name = file_path
            .clone()
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let file_name = file_path.to_str().unwrap().to_string();
        let key_name = cli.object_name.clone();
        let mut handles = vec![];
        for item_paths in item_path_list {
            let item_paths = item_paths.clone();
            let file_name = file_name.clone();
            let key_name = key_name.clone();
            let qiniu = qiniu.clone();
            let dir_name = dir_name.clone();
            let part_size = cli.part_size;
            let domain_name = cli.domain_name.clone();
            let handle = tokio::spawn(async move {
                let mut success = 0;
                let mut fail = 0;
                for item in item_paths {
                    let mut object_name = item.to_str().unwrap().to_string();
                    if let Some(ref dest_dir) = key_name {
                        let dest_dir = dest_dir.strip_prefix("/").unwrap_or(dest_dir);
                        let dest_dir = dest_dir.strip_suffix("/").unwrap_or(dest_dir);
                        object_name = format!(
                            "{dest_dir}/{dir_name}/{}",
                            object_name.strip_prefix(&file_name).unwrap_or(&object_name)
                        );
                        object_name = object_name.replace("//", "/").to_lowercase();
                    } else {
                        object_name = format!("uploads/{object_name}")
                            .replace("//", "/")
                            .to_lowercase();
                    }
                    let file = fs::File::open(item.clone()).await.unwrap();
                    let file_size = file.metadata().await.unwrap().size();
                    let mut error_message = String::new();
                    #[cfg(feature = "progress-bar")]
                    match qiniu
                        .clone()
                        .part_upload_file_no_progress_bar(
                            &object_name,
                            file,
                            file_size as usize,
                            part_size,
                            Some(1),
                        )
                        .await
                    {
                        Ok(_) => success += 1,
                        Err(e) => {
                            fail += 1;
                            error_message = e.to_string();
                        }
                    }
                    #[cfg(not(feature = "progress-bar"))]
                    match qiniu
                        .clone()
                        .part_upload_file(
                            &object_name,
                            file,
                            file_size as usize,
                            part_size,
                            Some(1),
                        )
                        .await
                    {
                        Ok(_) => success += 1,
                        Err(e) => {
                            fail += 1;
                            error_message = e.to_string();
                        }
                    }
                    if !error_message.is_empty() {
                        eprintln!(
                            "😭 {} -> {} 上传失败, {}",
                            item.to_str().unwrap().green(),
                            object_name.yellow(),
                            error_message.red(),
                        );
                    } else {
                        println!(
                            "🚀 {} -> {} 上传成功",
                            item.to_str().unwrap().green(),
                            object_name.yellow(),
                        );
                        let download_url = get_download_url(domain_name.clone(), &object_name);
                        if !download_url.is_empty() {
                            println!("🔗 {}\n", download_url.yellow());
                        }
                    }
                }
                (success, fail)
            });
            handles.push(handle);
        }
        let mut success = 0;
        let mut fail = 0;
        for handle in handles {
            let res = handle.await.unwrap();
            success += res.0;
            fail += res.1;
        }
        println!(
            "🚀 文件夹 {} 上传完成\n🔥 {} 个文件上传成功, {} 个文件上传失败, {:.2}s elapsed.",
            file_path.to_str().unwrap().green(),
            success.to_string().green(),
            fail.to_string().red(),
            start.elapsed().as_secs_f64(),
        );
        return Ok(());
    }
    let file = File::open(&file_path).await.unwrap_or_else(|_| {
        eprintln!(
            "{}",
            format!("read {} failed !", file_path.to_str().unwrap()).red()
        );
        exit(1);
    });
    let object_name = match cli.object_name.clone() {
        Some(name) => name,
        None => {
            format!(
                "uploads/{}",
                file_path.file_name().unwrap().to_str().unwrap()
            )
        }
    };
    // size in bytes
    let size = file.metadata().await.unwrap().size();
    match upload_to_qiniu(
        qiniu,
        file,
        object_name.as_str(),
        size as usize,
        cli.part_size,
        cli.threads,
    )
    .await
    {
        Ok(()) => {
            println!(
                "🚀 {} -> {} 上传成功",
                file_path.to_str().unwrap().green(),
                object_name.yellow(),
            );
        }
        Err(e) => {
            eprintln!(
                "😭 {} -> {} 上传失败, {}",
                file_path.to_str().unwrap().green(),
                object_name.yellow(),
                e.to_string().red()
            );
        }
    };
    let download_url = get_download_url(cli.domain_name, &object_name);
    if !download_url.is_empty() {
        println!("🔗 {}", download_url.yellow());
        if !cli.no_qrcode {
            let code = QrCode::new(download_url).unwrap();
            let image = code
                .render::<unicode::Dense1x2>()
                .module_dimensions(1, 1)
                .dark_color(unicode::Dense1x2::Light)
                .light_color(unicode::Dense1x2::Dark)
                .build();
            println!("{}", image);
        }
    }
    println!(
        "{}",
        format!("{:.2}s elapsed.", start.elapsed().as_secs_f64()).cyan()
    );
    Ok(())
}
