use super::*;

const MAGIC: &[u8; 4] = b"ATH1";
const BEDS: &[&str] = &[
    "L", "R", "C", "LFE", "LFE2", "Ls", "Rs", "Lb", "Rb", "Tfl", "Tfr", "Tbl", "Tbr", "Tml", "Tmr",
    "Lw", "Rw",
];
const INVALID: RenderError = RenderError::InvalidMetadata;

fn write_role(out: &mut Vec<u8>, role: Role) -> Result<(), RenderError> {
    let id = match role {
        Role::Object => 0,
        Role::Bed(name) => BEDS.iter().position(|&bed| bed == name).ok_or(INVALID)? + 1,
    };
    out.push(id as u8);
    Ok(())
}

fn write_floats(out: &mut Vec<u8>, values: &[f32]) {
    for value in values {
        out.extend(value.to_le_bytes());
    }
}

fn write_state(out: &mut Vec<u8>, state: ElementState) {
    write_floats(out, &state.position);
    write_floats(out, &[state.gain]);
}

pub(crate) fn encode_checkpoint(
    selection: usize,
    renderer: &ObjectRenderer,
) -> Result<Vec<u8>, RenderError> {
    let mut out = MAGIC.to_vec();
    out.extend([
        selection as u8,
        u8::from(renderer.layout == Layout::SevenOneFour)
            | (u8::from(renderer.allow_elevation_exclusion) << 1),
        renderer.roles.len() as u8,
        renderer.pending.len() as u8,
    ]);
    out.extend(renderer.applied.to_le_bytes());
    for (&role, gain) in renderer.roles.iter().zip(&renderer.gains) {
        write_role(&mut out, role)?;
        out.extend(gain.total.to_le_bytes());
        out.extend(gain.remaining.to_le_bytes());
        out.push(u8::from(gain.state.is_some()));
        if let Some(state) = gain.state {
            write_state(&mut out, state);
        }
        write_floats(&mut out, &gain.origin[..renderer.layout.channels()]);
        write_floats(&mut out, &gain.target[..renderer.layout.channels()]);
    }
    for update in &renderer.pending {
        if update.roles.len() != renderer.roles.len() {
            return Err(INVALID);
        }
        out.extend(update.frame.to_le_bytes());
        out.extend(update.ramp.to_le_bytes());
        for (&role, &state) in update.roles.iter().zip(&update.states) {
            write_role(&mut out, role)?;
            write_state(&mut out, state);
        }
    }
    Ok(out)
}

struct Reader<'a>(&'a [u8]);
impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], RenderError> {
        let (head, tail) = self.0.split_at_checked(N).ok_or(INVALID)?;
        self.0 = tail;
        head.try_into().map_err(|_| INVALID)
    }

    fn float(&mut self) -> Result<f32, RenderError> {
        let value = f32::from_le_bytes(self.take()?);
        if !value.is_finite() || value.abs() > 1e12 {
            return Err(INVALID);
        }
        Ok(value)
    }

    fn role(&mut self) -> Result<Role, RenderError> {
        match self.take::<1>()?[0] {
            0 => Ok(Role::Object),
            id => BEDS
                .get(usize::from(id - 1))
                .copied()
                .map(Role::Bed)
                .ok_or(INVALID),
        }
    }

    fn state(&mut self) -> Result<ElementState, RenderError> {
        Ok(ElementState {
            position: [self.float()?, self.float()?, self.float()?],
            gain: self.float()?,
        })
    }
}

pub(crate) fn decode_checkpoint(bytes: &[u8]) -> Result<(usize, ObjectRenderer), RenderError> {
    let mut reader = Reader(bytes);
    if &reader.take::<4>()? != MAGIC {
        return Err(INVALID);
    }
    let [selection, flags, channels, pending] = reader.take()?;
    let layout = match flags & !2 {
        0 => Layout::FiveOneFour,
        1 => Layout::SevenOneFour,
        _ => return Err(INVALID),
    };
    if selection > 3 || !(1..=16).contains(&channels) || pending > 64 {
        return Err(INVALID);
    }
    let mut renderer = ObjectRenderer::new(layout);
    renderer.allow_elevation_exclusion = flags & 2 != 0;
    renderer.next_frame = Some(0);
    renderer.applied = u64::from_le_bytes(reader.take()?);
    if renderer.applied > u64::from(u32::MAX) {
        return Err(INVALID);
    }
    for _ in 0..channels {
        renderer.roles.push(reader.role()?);
        let total = u32::from_le_bytes(reader.take()?);
        let remaining = u32::from_le_bytes(reader.take()?);
        if remaining > total {
            return Err(INVALID);
        }
        let state = match reader.take::<1>()?[0] {
            0 => None,
            1 => Some(reader.state()?),
            _ => return Err(INVALID),
        };
        let mut origin = [0.; MAX_LAYOUT_CHANNELS];
        let mut target = [0.; MAX_LAYOUT_CHANNELS];
        for value in origin[..layout.channels()]
            .iter_mut()
            .chain(&mut target[..layout.channels()])
        {
            *value = reader.float()?;
        }
        renderer.gains.push(Gain {
            origin,
            target,
            total,
            remaining,
            state,
        });
    }
    for _ in 0..pending {
        let frame = u64::from_le_bytes(reader.take()?);
        let ramp = u32::from_le_bytes(reader.take()?);
        let mut roles = Vec::with_capacity(channels as usize);
        let mut states = Vec::with_capacity(channels as usize);
        for _ in 0..channels {
            roles.push(reader.role()?);
            states.push(reader.state()?);
        }
        renderer.schedule(TimedUpdate {
            frame,
            ramp,
            roles,
            states,
        })?;
    }
    if !reader.0.is_empty() {
        return Err(INVALID);
    }
    Ok((selection as usize, renderer))
}
