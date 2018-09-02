import unittest
from types import SimpleNamespace

from migen import *

from artiq.gateware.rtio import cri
from artiq.gateware.test.drtio.packet_interface import PacketInterface
from artiq.gateware.drtio.rt_packet_repeater import RTPacketRepeater


def create_dut(nwords):
    pt = PacketInterface("s2m", nwords*8)
    pr = PacketInterface("m2s", nwords*8)
    dut = ClockDomainsRenamer({"rtio": "sys", "rtio_rx": "sys"})(
        RTPacketRepeater(SimpleNamespace(
            rx_rt_frame=pt.frame, rx_rt_data=pt.data,
            tx_rt_frame=pr.frame, tx_rt_data=pr.data)))
    return pt, pr, dut


class TestRepeater(unittest.TestCase):
    def test_output(self):
        test_writes = [
            (1, 10, 21, 0x42),
            (2, 11, 34, 0x2342),
            (3, 12, 83, 0x2345566633),
            (4, 13, 25, 0x98da14959a19498ae1),
            (5, 14, 75, 0x3998a1883ae14f828ae24958ea2479)
        ]

        for nwords in range(1, 8):
            pt, pr, dut = create_dut(nwords)

            def send():
                for channel, timestamp, address, data in test_writes:
                    yield dut.cri.chan_sel.eq(channel)
                    yield dut.cri.timestamp.eq(timestamp)
                    yield dut.cri.o_address.eq(address)
                    yield dut.cri.o_data.eq(data)
                    yield dut.cri.cmd.eq(cri.commands["write"])
                    yield
                    yield dut.cri.cmd.eq(cri.commands["nop"])
                    yield
                    for i in range(30):
                        yield
                for i in range(50):
                        yield

            short_data_len = pr.plm.field_length("write", "short_data")

            received = []
            def receive(packet_type, field_dict, trailer):
                self.assertEqual(packet_type, "write")
                self.assertEqual(len(trailer), field_dict["extra_data_cnt"]) 
                data = field_dict["short_data"]
                for n, te in enumerate(trailer):
                    data |= te << (n*nwords*8 + short_data_len)
                received.append((field_dict["channel"], field_dict["timestamp"],
                                 field_dict["address"], data))

            run_simulation(dut, [send(), pr.receive(receive)])
            self.assertEqual(test_writes, received)

    def test_buffer_space(self):
        for nwords in range(1, 8):
            pt, pr, dut = create_dut(nwords)

            def send_requests():
                for i in range(10):
                    yield dut.cri.chan_sel.eq(i << 16)
                    yield dut.cri.cmd.eq(cri.commands["get_buffer_space"])
                    yield
                    yield dut.cri.cmd.eq(cri.commands["nop"])
                    yield
                    while not (yield dut.cri.o_buffer_space_valid):
                        yield
                    buffer_space = yield dut.cri.o_buffer_space
                    self.assertEqual(buffer_space, 2*i)

            current_request = None

            @passive
            def send_replies():
                nonlocal current_request
                while True:
                    while current_request is None:
                        yield
                    yield from pt.send("buffer_space_reply", space=2*current_request)
                    current_request = None

            def receive(packet_type, field_dict, trailer):
                nonlocal current_request
                self.assertEqual(packet_type, "buffer_space_request")
                self.assertEqual(trailer, [])
                self.assertEqual(current_request, None)
                current_request = field_dict["destination"]

            run_simulation(dut, [send_requests(), send_replies(), pr.receive(receive)])
