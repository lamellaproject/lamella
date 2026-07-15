// Lamella.Net.Time.Nts -- the RFC 8915 wire codec: NTS-KE records and NTP extension fields.
using System;

namespace Lamella.Net.Time
{
    internal sealed class NtsProtocol
    {
        internal const int RecordEndOfMessage = 0;
        internal const int RecordNextProtocol = 1;
        internal const int RecordError = 2;
        internal const int RecordWarning = 3;
        internal const int RecordAeadAlgorithm = 4;
        internal const int RecordNewCookie = 5;
        internal const int RecordNtpv4Server = 6;
        internal const int RecordNtpv4Port = 7;

        internal const int NextProtocolNtpv4 = 0;
        internal const int AeadAesSivCmac256 = 15;

        internal const int FieldUniqueIdentifier = 0x0104;
        internal const int FieldNtsCookie = 0x0204;
        internal const int FieldCookiePlaceholder = 0x0304;
        internal const int FieldNtsAuthenticator = 0x0404;

        internal static byte[] ExporterContext(bool clientToServer)
        {
            byte[] context = new byte[5];
            context[0] = 0;
            context[1] = (byte)NextProtocolNtpv4;
            context[2] = (byte)(AeadAesSivCmac256 >> 8);
            context[3] = (byte)AeadAesSivCmac256;
            context[4] = clientToServer ? (byte)0x00 : (byte)0x01;
            return context;
        }

        internal static int WriteRecord(byte[] buffer, int offset, bool critical, int type, byte[] body, int bodyLen)
        {
            int typeField = type | (critical ? 0x8000 : 0);
            buffer[offset] = (byte)(typeField >> 8);
            buffer[offset + 1] = (byte)typeField;
            buffer[offset + 2] = (byte)(bodyLen >> 8);
            buffer[offset + 3] = (byte)bodyLen;
            for (int i = 0; i < bodyLen; i++) buffer[offset + 4 + i] = body[i];
            return offset + 4 + bodyLen;
        }

        internal static byte[] BuildKeRequest()
        {
            byte[] buffer = new byte[64];
            byte[] two = new byte[2];
            int offset = 0;
            two[0] = 0; two[1] = (byte)NextProtocolNtpv4;
            offset = WriteRecord(buffer, offset, true, RecordNextProtocol, two, 2);
            two[0] = (byte)(AeadAesSivCmac256 >> 8); two[1] = (byte)AeadAesSivCmac256;
            offset = WriteRecord(buffer, offset, true, RecordAeadAlgorithm, two, 2);
            offset = WriteRecord(buffer, offset, true, RecordEndOfMessage, new byte[0], 0);
            byte[] request = new byte[offset];
            for (int i = 0; i < offset; i++) request[i] = buffer[i];
            return request;
        }

        internal static int WriteField(byte[] buffer, int offset, int type, byte[] body, int bodyLen)
        {
            int total = 4 + bodyLen;
            int padded = (total + 3) & ~3;
            buffer[offset] = (byte)(type >> 8);
            buffer[offset + 1] = (byte)type;
            buffer[offset + 2] = (byte)(padded >> 8);
            buffer[offset + 3] = (byte)padded;
            for (int i = 0; i < bodyLen; i++) buffer[offset + 4 + i] = body[i];
            for (int i = total; i < padded; i++) buffer[offset + i] = 0;
            return offset + padded;
        }

        internal static int ReadFieldType(byte[] packet, int offset)
        {
            if (offset + 4 > packet.Length) return -1;
            return (packet[offset] << 8) | packet[offset + 1];
        }

        internal static int ReadFieldLength(byte[] packet, int offset)
        {
            if (offset + 4 > packet.Length) return 0;
            int len = (packet[offset + 2] << 8) | packet[offset + 3];
            if (len < 4 || (len & 3) != 0) return 0;
            if (offset + len > packet.Length) return 0;
            return len;
        }
    }
}
