//! Small checked reader for the loose PC Modtools binary formats.

pub(crate) struct Reader<'a> {
    data: &'a [u8],
    pub position: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    pub fn bytes(&mut self, size: usize) -> Result<&'a [u8], String> {
        let end = self
            .position
            .checked_add(size)
            .ok_or("File offset overflow.")?;
        let value = self
            .data
            .get(self.position..end)
            .ok_or("Incomplete model data.")?;
        self.position = end;
        Ok(value)
    }

    pub fn skip(&mut self, size: usize) -> Result<(), String> {
        self.bytes(size)?;
        Ok(())
    }

    pub fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, String> {
        let value = self.bytes(2)?;
        Ok(u16::from_le_bytes([value[0], value[1]]))
    }

    pub fn i16(&mut self) -> Result<i16, String> {
        Ok(self.u16()? as i16)
    }

    pub fn u32(&mut self) -> Result<u32, String> {
        let value = self.bytes(4)?;
        Ok(u32::from_le_bytes(value.try_into().unwrap()))
    }

    pub fn f32(&mut self) -> Result<f32, String> {
        let value = f32::from_bits(self.u32()?);
        if !value.is_finite() {
            return Err("Non-finite position, normal or UV in model data.".into());
        }
        Ok(value)
    }

    pub fn vector(&mut self) -> Result<[f32; 3], String> {
        Ok([self.f32()?, self.f32()?, self.f32()?])
    }

    pub fn string(&mut self) -> Result<String, String> {
        let value = self.string_at(self.position)?;
        self.position += value.len() + 1;
        Ok(value)
    }

    pub fn string_at(&self, offset: usize) -> Result<String, String> {
        let remaining = self
            .data
            .get(offset..)
            .ok_or("String offset outside file.")?;
        let length = remaining
            .iter()
            .position(|byte| *byte == 0)
            .ok_or("Unterminated model string.")?;
        if length > 1024 {
            return Err("Model string exceeds the supported length.".into());
        }
        String::from_utf8(remaining[..length].to_vec()).map_err(|error| error.to_string())
    }

    pub fn finish(&self) -> Result<(), String> {
        if self.position != self.data.len() {
            return Err("Unexpected trailing model data.".into());
        }
        Ok(())
    }
}
