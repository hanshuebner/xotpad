use libxotpad::x3::X3ParamError;
use std::ops::Deref;

const PARAMS: [u8; 3] = [16, 17, 18];

#[derive(Clone, Debug)]
pub struct UserPadParams {
    pub char_delete: X3CharDelete,

    pub line_delete: X3LineDelete,

    pub line_display: X3LineDisplay,
}

impl libxotpad::x3::X3Params for UserPadParams {
    fn get(&self, param: u8) -> Option<u8> {
        match param {
            16 => Some(*self.char_delete),
            17 => Some(*self.line_delete),
            18 => Some(*self.line_display),
            _ => None,
        }
    }

    fn set(&mut self, param: u8, value: u8) -> Result<(), X3ParamError> {
        match param {
            16 => self.char_delete = X3CharDelete::try_from(value)?,
            17 => self.line_delete = X3LineDelete::try_from(value)?,
            18 => self.line_display = X3LineDisplay::try_from(value)?,
            _ => return Err(X3ParamError::Unsupported),
        };

        Ok(())
    }

    fn all(&self) -> Vec<(u8, u8)> {
        let mut params = Vec::new();

        for param in PARAMS {
            params.push((param, self.get(param).unwrap()));
        }

        params
    }
}

#[derive(Copy, Clone, Debug)]
pub struct X3CharDelete(u8);

impl Deref for X3CharDelete {
    type Target = u8;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TryFrom<u8> for X3CharDelete {
    type Error = X3ParamError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            127 => Ok(X3CharDelete(value)),
            _ => Err(X3ParamError::InvalidValue),
        }
    }
}

impl X3CharDelete {
    pub fn is_match(&self, byte: u8) -> bool {
        byte == self.0
    }
}

#[derive(Copy, Clone, Debug)]
pub struct X3LineDelete(u8);

impl Deref for X3LineDelete {
    type Target = u8;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TryFrom<u8> for X3LineDelete {
    type Error = X3ParamError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value > 127 {
            return Err(X3ParamError::InvalidValue);
        }

        Ok(X3LineDelete(value))
    }
}

impl X3LineDelete {
    pub fn is_match(&self, byte: u8) -> bool {
        byte == self.0
    }
}

#[derive(Copy, Clone, Debug)]
pub struct X3LineDisplay(u8);

impl Deref for X3LineDisplay {
    type Target = u8;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TryFrom<u8> for X3LineDisplay {
    type Error = X3ParamError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        if value > 127 {
            return Err(X3ParamError::InvalidValue);
        }

        Ok(X3LineDisplay(value))
    }
}

impl X3LineDisplay {
    pub fn is_match(&self, byte: u8) -> bool {
        byte == self.0
    }
}
