use std::io::Write;

use flate2::{
    write::{ZlibDecoder, ZlibEncoder},
    Compression,
};

use crate::flasher::FlashSize;
use crate::{
    command::{Command, CommandType},
    connection::{Connection, USB_SERIAL_JTAG_PID},
    elf::RomSegment,
    error::Error,
    flasher::{ProgressCallbacks, SpiAttachParams, FLASH_SECTOR_SIZE},
    targets::Chip,
    targets::FlashTarget,
};

/// Applications running from an ESP32's (or variant's) flash
pub struct Esp32Target {
    chip: Chip,
    spi_attach_params: SpiAttachParams,
    flash_size: FlashSize,
    use_stub: bool,
    encrypt_flash: bool,
}

impl Esp32Target {
    pub fn new(
        chip: Chip,
        spi_attach_params: SpiAttachParams,
        flash_size: FlashSize,
        use_stub: bool,
        encrypt_flash: bool,
    ) -> Self {
        Esp32Target {
            chip,
            spi_attach_params,
            flash_size,
            use_stub,
            encrypt_flash,
        }
    }
}

impl FlashTarget for Esp32Target {
    fn begin(&mut self, connection: &mut Connection) -> Result<(), Error> {
        connection.with_timeout(CommandType::SpiSetParams.timeout(), |connection| {
            connection.command(Command::SpiSetParams {
                // These values are from esptool, most of them are just hardcoded
                flash_id: 0,
                size: self.flash_size.size(),
                block_size: 64 * 1024,
                sector_size: 4 * 1024,
                page_size: 256,
                status_mask: 0xFFFF,
            })
        })?;

        connection.with_timeout(CommandType::SpiAttach.timeout(), |connection| {
            let command = if self.use_stub {
                Command::SpiAttachStub {
                    spi_params: self.spi_attach_params,
                }
            } else {
                Command::SpiAttach {
                    spi_params: self.spi_attach_params,
                }
            };

            connection.command(command)
        })?;

        // The stub usually disables these watchdog timers, however if we're not using the stub
        // we need to disable them before flashing begins
        // TODO: the stub doesn't appear to disable the watchdog on ESP32-S3, so we explicitly
        //       disable the watchdog here.
        if connection.get_usb_pid()? == USB_SERIAL_JTAG_PID {
            match self.chip {
                Chip::Esp32c3 => {
                    connection.command(Command::WriteReg {
                        address: 0x6000_80a8,
                        value: 0x50D8_3AA1,
                        mask: None,
                    })?; // WP disable
                    connection.command(Command::WriteReg {
                        address: 0x6000_8090,
                        value: 0x0,
                        mask: None,
                    })?; // turn off RTC WDT
                    connection.command(Command::WriteReg {
                        address: 0x6000_80a8,
                        value: 0x0,
                        mask: None,
                    })?; // WP enable
                }
                Chip::Esp32s3 => {
                    connection.command(Command::WriteReg {
                        address: 0x6000_80B0,
                        value: 0x50D8_3AA1,
                        mask: None,
                    })?; // WP disable
                    connection.command(Command::WriteReg {
                        address: 0x6000_8098,
                        value: 0x0,
                        mask: None,
                    })?; // turn off RTC WDT
                    connection.command(Command::WriteReg {
                        address: 0x6000_80B0,
                        value: 0x0,
                        mask: None,
                    })?; // WP enable
                }
                Chip::Esp32c6 => {
                    connection.command(Command::WriteReg {
                        address: 0x600B_1C18,
                        value: 0x50D8_3AA1,
                        mask: None,
                    })?; // WP disable
                    connection.command(Command::WriteReg {
                        address: 0x600B_1C00,
                        value: 0x0,
                        mask: None,
                    })?; // turn off RTC WDT
                    connection.command(Command::WriteReg {
                        address: 0x600B_1C18,
                        value: 0x0,
                        mask: None,
                    })?; // WP enable
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn write_segment(
        &mut self,
        connection: &mut Connection,
        segment: RomSegment,
        progress: &mut Option<&mut dyn ProgressCallbacks>,
    ) -> Result<(), Error> {
        let addr = segment.addr;

        let target = self.chip.into_target();
        let flash_write_size = target.flash_write_size(connection)?;
        let erase_count = (segment.data.len() + FLASH_SECTOR_SIZE - 1) / FLASH_SECTOR_SIZE;

        // round up to sector size
        let mut erase_size = (erase_count * FLASH_SECTOR_SIZE) as u32;
        if self.encrypt_flash {
            // round up to a multiple of 32
            erase_size = (erase_size + 31) & !31;
        } else {
            // round up to a multiple of 4
            erase_size = (erase_size + 3) & !3;
        }

        if connection.should_use_compression() {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
            encoder.write_all(&segment.data)?;
            let compressed = encoder.finish()?;

            let block_count = (compressed.len() + flash_write_size - 1) / flash_write_size;

            connection.with_timeout(
                CommandType::FlashDeflateBegin.timeout_for_size(erase_size),
                |connection| {
                    connection.command(Command::FlashDeflateBegin {
                        size: segment.data.len() as u32,
                        blocks: block_count as u32,
                        block_size: flash_write_size as u32,
                        offset: addr,
                        supports_encryption: self.chip != Chip::Esp32 && !self.use_stub,
                    })?;
                    Ok(())
                },
            )?;

            let chunks = compressed.chunks(flash_write_size);
            let num_chunks = chunks.len();

            if let Some(cb) = progress.as_mut() {
                cb.init(addr, num_chunks)
            }

            // decode the chunks to see how much data the device will have to save
            let mut decoder = ZlibDecoder::new(Vec::new());
            let mut decoded_size = 0;

            for (i, block) in chunks.enumerate() {
                decoder.write_all(block)?;
                decoder.flush()?;
                let size = decoder.get_ref().len() - decoded_size;
                decoded_size = decoder.get_ref().len();

                connection.with_timeout(
                    CommandType::FlashDeflateData.timeout_for_size(size as u32),
                    |connection| {
                        connection.command(Command::FlashDeflateData {
                            sequence: i as u32,
                            pad_to: 0,
                            pad_byte: 0xff,
                            data: block,
                        })?;
                        Ok(())
                    },
                )?;

                if let Some(cb) = progress.as_mut() {
                    cb.update(i + 1)
                }
            }
        } else {
            let block_count = (segment.data.len() + flash_write_size - 1) / flash_write_size;

            connection.with_timeout(
                CommandType::FlashBegin.timeout_for_size(erase_size),
                |connection| {
                    connection.command(Command::FlashBegin {
                        size: erase_size,
                        blocks: block_count as u32,
                        block_size: flash_write_size as u32,
                        offset: addr,
                        supports_encryption: self.chip != Chip::Esp32 && !self.use_stub,
                        encrypt: self.encrypt_flash,
                    })?;
                    Ok(())
                },
            )?;

            let chunks = segment.data.chunks(flash_write_size);
            let num_chunks = chunks.len();

            if let Some(cb) = progress.as_mut() {
                cb.init(addr, num_chunks)
            }

            for (i, block) in chunks.enumerate() {
                if self.encrypt_flash && (self.chip == Chip::Esp32 || self.use_stub) {
                    // We need to issue a special data command for encrypted flash
                    connection.with_timeout(
                        CommandType::FlashEncryptData.timeout_for_size(block.len() as u32),
                        |connection| {
                            connection.command(Command::FlashEncryptData {
                                sequence: i as u32,
                                pad_to: flash_write_size,
                                pad_byte: 0xff,
                                data: block,
                            })?;
                            Ok(())
                        },
                    )?;
                } else {
                    connection.with_timeout(
                        CommandType::FlashData.timeout_for_size(block.len() as u32),
                        |connection| {
                            connection.command(Command::FlashData {
                                sequence: i as u32,
                                pad_to: flash_write_size,
                                pad_byte: 0xff,
                                data: block,
                            })?;
                            Ok(())
                        },
                    )?;
                }

                if let Some(cb) = progress.as_mut() {
                    cb.update(i + 1)
                }
            }
        }

        if let Some(cb) = progress.as_mut() {
            cb.finish()
        }

        Ok(())
    }

    fn finish(&mut self, connection: &mut Connection, reboot: bool) -> Result<(), Error> {
        if connection.should_use_compression() {
            connection.with_timeout(CommandType::FlashDeflateEnd.timeout(), |connection| {
                connection.command(Command::FlashDeflateEnd { reboot: false })
            })?;
        } else {
            connection.with_timeout(CommandType::FlashEnd.timeout(), |connection| {
                connection.command(Command::FlashEnd { reboot: false })
            })?;
        }

        if reboot {
            connection.reset()?;
        }

        Ok(())
    }
}
