from artiq.language.core import *
from artiq.language.db import *
from artiq.language.units import *


PHASE_MODE_DEFAULT = -1
# keep in sync with dds.h
PHASE_MODE_CONTINUOUS = 0
PHASE_MODE_ABSOLUTE = 1
PHASE_MODE_TRACKING = 2


class DDS(AutoDB):
    """Core device Direct Digital Synthesis (DDS) driver.

    Controls DDS devices managed directly by the core device's runtime.

    :param dds_sysclk: DDS system frequency, used for computing the frequency
        tuning words.
    :param channel: channel number of the DDS device to control.
    """
    class DBKeys:
        core = Device()
        dds_sysclk = Argument(1*GHz)
        channel = Argument()

    def build(self):
        self.phase_mode = PHASE_MODE_CONTINUOUS

    @portable
    def frequency_to_ftw(self, frequency):
        """Returns the frequency tuning word corresponding to the given
        frequency.
        """
        return round(2**32*frequency/self.dds_sysclk)

    @portable
    def ftw_to_frequency(self, ftw):
        """Returns the frequency corresponding to the given frequency tuning
        word.
        """
        return ftw*self.dds_sysclk/2**32

    @kernel
    def init(self):
        """Resets and initializes the DDS."""
        syscall("dds_init", time_to_cycles(now()), self.channel)

    @kernel
    def set_phase_mode(self, phase_mode):
        """Sets the phase mode of the DDS channel. Supported phase modes are:

        * ``PHASE_MODE_CONTINUOUS``: the phase accumulator is unchanged when
          switching frequencies. The DDS phase is the sum of the phase
          accumulator and the phase offset. The only discrete jumps in the
          DDS output phase come from changes to the phase offset.

        * ``PHASE_MODE_ABSOLUTE``: the phase accumulator is reset when
          switching frequencies. Thus, the phase of the DDS at the time of
          the frequency change is equal to the phase offset.

        * ``PHASE_MODE_TRACKING``: when switching frequencies, the phase
          accumulator is set to the value it would have if the DDS had been
          running at the specified frequency since the start of the
          experiment.
        """
        self.phase_mode = phase_mode

    @kernel
    def set(self, frequency, phase_mode=PHASE_MODE_DEFAULT, phase_offset=0):
        """Sets the DDS channel to the specified frequency and phase.

        :param frequency: frequency to generate.
        :param phase_mode: if specified, overrides the default phase mode set
            by ``set_phase_mode`` for this call.
        :param phase_offset: adds an offset, in turns, to the phase.
        """
        if phase_mode == PHASE_MODE_DEFAULT:
            phase_mode = self.phase_mode

        syscall("dds_set", time_to_cycles(now()), self.channel,
           self.frequency_to_ftw(frequency), round(phase_offset*2**14),
           self.phase_mode)
