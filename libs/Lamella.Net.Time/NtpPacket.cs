// Lamella.Net.Time -- the shared NTPv4 packet codec (RFC 4330 header math).
using System;

namespace Lamella.Net.Time
{
    public sealed class NtpPacket
    {
        private NtpPacket() { }

        public static byte[] BuildClientHeader(long utcTicksT1)
        {
            byte[] header = new byte[48];
            header[0] = 0x23;
            WriteTimestamp(header, 40, utcTicksT1);
            return header;
        }

        public static SyncResult Evaluate(
            byte[] request, byte[] reply, long t1, long t4, string protocol, bool authenticated)
        {
            if (reply.Length < 48)
                return Fail(protocol, "short reply (" + reply.Length.ToString() + " bytes)");

            int li = (reply[0] >> 6) & 3;
            int mode = reply[0] & 7;
            int stratum = reply[1];

            if (mode != 4) return Fail(protocol, "not a server reply (mode " + mode.ToString() + ")");
            if (stratum == 0) return Fail(protocol, "kiss-of-death from server: " + KissCode(reply));
            if (stratum > 15) return Fail(protocol, "invalid stratum " + stratum.ToString());
            if (li == 3) return Fail(protocol, "server clock not synchronized");
            for (int i = 0; i < 8; i++)
            {
                if (reply[24 + i] != request[40 + i]) return Fail(protocol, "originate timestamp mismatch");
            }
            bool transmitZero = true;
            for (int i = 0; i < 8; i++)
            {
                if (reply[40 + i] != 0) transmitZero = false;
            }
            if (transmitZero) return Fail(protocol, "zero transmit timestamp");

            long t2 = ReadTimestampTicks(reply, 32);
            long t3 = ReadTimestampTicks(reply, 40);
            long roundTrip = (t4 - t1) - (t3 - t2);
            if (roundTrip < 0) roundTrip = 0;
            long offset = ((t2 - t1) + (t3 - t4)) / 2;
            long corrected = t4 + offset;
            if (corrected <= 0) return Fail(protocol, "implausible server time");

            string warning = null;
            if (li == 1) warning = "leap second insertion pending";
            else if (li == 2) warning = "leap second deletion pending";

            DateTime utc = new DateTime(corrected);
            SystemClock.SetUtc(utc);
            return new SyncResult(
                true, protocol, authenticated, stratum,
                new TimeSpan(offset), new TimeSpan(roundTrip), utc, warning);
        }

        public static long ReadTimestampTicks(byte[] packet, int offset)
        {
            ulong seconds = ((ulong)packet[offset] << 24) | ((ulong)packet[offset + 1] << 16)
                | ((ulong)packet[offset + 2] << 8) | (ulong)packet[offset + 3];
            ulong fraction = ((ulong)packet[offset + 4] << 24) | ((ulong)packet[offset + 5] << 16)
                | ((ulong)packet[offset + 6] << 8) | (ulong)packet[offset + 7];
            long fractionTicks = (long)((fraction * 10000000UL) >> 32);
            long eraBase = new DateTime(1900, 1, 1).Ticks;
            if ((seconds & 0x80000000UL) == 0)
            {
                eraBase = eraBase + 4294967296L * DateTime.TicksPerSecond;
            }
            return eraBase + (long)seconds * DateTime.TicksPerSecond + fractionTicks;
        }

        public static void WriteTimestamp(byte[] packet, int offset, long utcTicks)
        {
            long since1900 = utcTicks - new DateTime(1900, 1, 1).Ticks;
            if (since1900 < 0) return;
            ulong seconds = (ulong)(since1900 / DateTime.TicksPerSecond) & 0xFFFFFFFFUL;
            long remainderTicks = since1900 % DateTime.TicksPerSecond;
            ulong fraction = ((ulong)remainderTicks << 32) / 10000000UL;
            packet[offset] = (byte)(seconds >> 24);
            packet[offset + 1] = (byte)(seconds >> 16);
            packet[offset + 2] = (byte)(seconds >> 8);
            packet[offset + 3] = (byte)seconds;
            packet[offset + 4] = (byte)(fraction >> 24);
            packet[offset + 5] = (byte)(fraction >> 16);
            packet[offset + 6] = (byte)(fraction >> 8);
            packet[offset + 7] = (byte)fraction;
        }

        private static string KissCode(byte[] reply)
        {
            string code = "";
            for (int i = 12; i < 16; i++)
            {
                int c = reply[i];
                if (c >= 0x20 && c < 0x7F) code = code + ((char)c).ToString();
            }
            return code.Length == 0 ? "(none)" : code;
        }

        private static SyncResult Fail(string protocol, string warning)
        {
            return new SyncResult(
                false, protocol, false, 0,
                new TimeSpan(0), new TimeSpan(0), new DateTime(0), warning);
        }
    }
}
