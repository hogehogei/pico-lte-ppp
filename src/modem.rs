
use embedded_io_async::{Read, Write};
use anyhow::{anyhow};
use alloc::string::String;
use core::str;

use embassy_time::{Timer, Duration, with_timeout};

#[derive(PartialEq)]
enum ModelReadSeq
{
    BeginCR,
    BeginLF,
    Data,
    EndLF,
}

static RETRY_TIME: i32 = 5;

async fn wait_readline<T: Read>(serial_port: &mut T, timeout: u64) -> anyhow::Result<String>
{
    let mut buf = [0; 1];
    let mut response : String = String::from("");
    let mut step = ModelReadSeq::BeginCR;

    loop {
        let read_result = with_timeout(Duration::from_millis(timeout), serial_port.read(&mut buf))
            .await
            .map_err(|_| anyhow!("serial_port read timeout."))?;
        
        match read_result {
            Ok(n) => {
                if n == 0 {
                    break;
                }
            }
            Err(e) => {
                return Err(anyhow!("serial_port read() error."));
            } 
        }

        match step {
            ModelReadSeq::BeginCR => { 
                if buf[0] == b'\r' {
                    step = ModelReadSeq::BeginLF;
                }
            },
            ModelReadSeq::BeginLF => { 
                if buf[0] == b'\n' {
                    step = ModelReadSeq::Data;
                }
            },
            ModelReadSeq::Data => {
                if buf[0] == b'\r' {
                    step = ModelReadSeq::EndLF;
                }
                else {
                    response += core::str::from_utf8(&buf).map_err(|_| anyhow!("convert byte to utf8 failed."))?;
                }
            },
            ModelReadSeq::EndLF => {
                if buf[0] == b'\n' {
                    break;
                }
            },
        }
    }

    if step != ModelReadSeq::EndLF {
        log::warn!("serial_port read cannot complete: {}", response);
        return Ok(String::from(""))
    }
    
    Ok(response)
}

async fn wait_response<T: Read>(serial_port: &mut T, timeout: u64) -> anyhow::Result<String>
{
    loop {
        let response = wait_readline(serial_port, timeout).await?;
        log::info!("Modem: [{}]", response);

        if response == "" {
            break;
        }
        if response == "OK" {
            return Ok(response);
        }
        if response.contains("CONNECT") {
            return Ok(response);
        }
        if response.contains("ERROR") {
            return Err(anyhow!("AT command error response : {}", response));
        }
    }

    Err(anyhow!("No correct response from modem."))
}

pub async fn send_cmd<T: Read + Write>(serial_port: &mut T, cmd: &str, timeout: u64) -> anyhow::Result<String>
{
    serial_port.write_all(cmd.as_bytes()).await.map_err(|_| anyhow!("send_cmd serial_port write error."))?;
    serial_port.flush().await.map_err(|_| anyhow!("send_cmd serial_port flush error."))?;
    let response = wait_response(serial_port, timeout).await?;

    Ok(response)
}

pub async fn send_cmd_retry<T: Read + Write>(serial_port: &mut T, cmd: &str, timeout: u64) -> anyhow::Result<String>
{
    let mut err_n = 0;
    let mut response: String = String::from("");
    
    for _i in 0..RETRY_TIME {
        match send_cmd(serial_port, cmd, timeout).await {
            Ok(s) => { 
                response = s; 
                break; 
            },
            Err(e) => { 
                err_n += 1; 
                log::warn!("init_lte_modem Error: \"{}\", retry cmd=\"{}\"", e, cmd);
            }
        }
    }
    if err_n >= RETRY_TIME {
        return Err(anyhow!("send_cmd retry failed. cmd=\"{}\"", cmd));
    }

    log::info!("send_cmd: \"{}\", returns \"{}\"", cmd, response);
    Ok(response)
}

pub async fn lte_initialize<T: Read + Write>(serial_port: &mut T) -> anyhow::Result<()>
{
    log::info!("init_lte_modem.");

    const CSQ : &str = "AT+CSQ\r";
    send_cmd_retry(serial_port, CSQ, 1000).await?;
    Timer::after(Duration::from_millis(200)).await;

    const ATE0 : &str = "ATE0\r";
    send_cmd_retry(serial_port, ATE0, 1000).await?;
    Timer::after(Duration::from_millis(200)).await;

    const CGDCONT : &str = "AT+CGDCONT=1,\"IP\",\"povo.jp\"\r";
    send_cmd_retry(serial_port, CGDCONT, 1000).await?;
    Timer::after(Duration::from_millis(200)).await;
    
    const ATD : &str = "ATD*99##\r";
    send_cmd_retry(serial_port, ATD, 1000).await?;
    Timer::after(Duration::from_millis(200)).await;

    Ok(())
}