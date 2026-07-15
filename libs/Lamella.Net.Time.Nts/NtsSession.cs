// Lamella.Net.Time.Nts -- the stateful NTS protocol object: keys + cookies + the protected NTP request/response codec (RFC 8915 sec 5).
using System;

namespace Lamella.Net.Time
{
    internal sealed class NtsSession
    {
        private readonly int _c2sKey;
        private readonly int _s2cKey;
        private byte[][] _cookies;
        private int _cookieCount;
        private byte[] _lastUniqueId;
        private readonly string _ntpServer;
        private readonly int _ntpPort;

        internal NtsSession(int c2sKey, int s2cKey, byte[][] cookies, int cookieCount)
            : this(c2sKey, s2cKey, cookies, cookieCount, null, 0) { }

        internal NtsSession(
            int c2sKey, int s2cKey, byte[][] cookies, int cookieCount, string ntpServer, int ntpPort)
        {
            _c2sKey = c2sKey;
            _s2cKey = s2cKey;
            _cookies = cookies;
            _cookieCount = cookieCount;
            _lastUniqueId = null;
            _ntpServer = ntpServer;
            _ntpPort = ntpPort;
        }

        internal string NtpServer
        {
            get { return _ntpServer; }
        }

        internal int NtpPort
        {
            get { return _ntpPort; }
        }

        internal int CookieCount
        {
            get { return _cookieCount; }
        }

        internal byte[] BuildRequest(byte[] header48, byte[] uniqueId, byte[] nonce, int placeholders)
        {
            if (_cookieCount <= 0) return null;
            byte[] cookie = TakeCookie();
            _lastUniqueId = uniqueId;

            byte[] scratch = new byte[512];
            int offset = 0;
            for (int i = 0; i < 48; i++) scratch[offset + i] = header48[i];
            offset += 48;
            offset = NtsProtocol.WriteField(scratch, offset, NtsProtocol.FieldUniqueIdentifier, uniqueId, uniqueId.Length);
            offset = NtsProtocol.WriteField(scratch, offset, NtsProtocol.FieldNtsCookie, cookie, cookie.Length);
            byte[] placeholder = new byte[cookie.Length];
            for (int i = 0; i < placeholders; i++)
            {
                offset = NtsProtocol.WriteField(scratch, offset, NtsProtocol.FieldCookiePlaceholder, placeholder, placeholder.Length);
            }

            byte[] associated = new byte[offset];
            for (int i = 0; i < offset; i++) associated[i] = scratch[i];

            byte[] sealedTag = new byte[16];
            if (NtsNative.SivEncrypt(_c2sKey, associated, nonce, new byte[0], sealedTag) < 0) return null;

            offset = NtsProtocol.WriteField(scratch, offset, NtsProtocol.FieldNtsAuthenticator,
                BuildAuthenticatorBody(nonce, sealedTag), 4 + PadTo4(nonce.Length) + PadTo4(sealedTag.Length));

            byte[] datagram = new byte[offset];
            for (int i = 0; i < offset; i++) datagram[i] = scratch[i];
            return datagram;
        }

        internal bool VerifyResponse(byte[] packet)
        {
            if (packet.Length < 48) return false;
            int authOffset = -1;
            byte[] echoedUniqueId = null;

            int offset = 48;
            while (offset + 4 <= packet.Length)
            {
                int type = NtsProtocol.ReadFieldType(packet, offset);
                int len = NtsProtocol.ReadFieldLength(packet, offset);
                if (len == 0) return false;
                if (type == NtsProtocol.FieldUniqueIdentifier)
                {
                    echoedUniqueId = Slice(packet, offset + 4, len - 4);
                }
                else if (type == NtsProtocol.FieldNtsAuthenticator)
                {
                    authOffset = offset;
                    break;
                }
                offset += len;
            }
            if (authOffset < 0 || echoedUniqueId == null) return false;
            if (!SameBytes(echoedUniqueId, _lastUniqueId)) return false;

            int bodyStart = authOffset + 4;
            int nonceLen = (packet[bodyStart] << 8) | packet[bodyStart + 1];
            int cipherLen = (packet[bodyStart + 2] << 8) | packet[bodyStart + 3];
            int nonceStart = bodyStart + 4;
            int cipherStart = nonceStart + PadTo4(nonceLen);
            byte[] nonce = Slice(packet, nonceStart, nonceLen);
            byte[] sealedData = Slice(packet, cipherStart, cipherLen);

            byte[] associated = Slice(packet, 0, authOffset);
            if (sealedData.Length < 16) return false;
            byte[] plaintext = new byte[sealedData.Length - 16];
            if (NtsNative.SivDecrypt(_s2cKey, associated, nonce, sealedData, plaintext) < 0) return false;

            HarvestCookies(plaintext);
            return true;
        }

        internal byte[] BuildResponse(byte[] header48, byte[] uniqueId, byte[] nonce, byte[][] freshCookies, int freshCount)
        {
            byte[] scratch = new byte[512];
            int offset = 0;
            for (int i = 0; i < 48; i++) scratch[offset + i] = header48[i];
            offset += 48;
            offset = NtsProtocol.WriteField(scratch, offset, NtsProtocol.FieldUniqueIdentifier, uniqueId, uniqueId.Length);
            byte[] associated = Slice(scratch, 0, offset);

            byte[] plainScratch = new byte[256];
            int plainOffset = 0;
            for (int i = 0; i < freshCount; i++)
            {
                plainOffset = NtsProtocol.WriteField(plainScratch, plainOffset, NtsProtocol.FieldNtsCookie, freshCookies[i], freshCookies[i].Length);
            }
            byte[] plaintext = Slice(plainScratch, 0, plainOffset);
            byte[] sealedData = new byte[plaintext.Length + 16];
            if (NtsNative.SivEncrypt(_s2cKey, associated, nonce, plaintext, sealedData) < 0) return null;

            offset = NtsProtocol.WriteField(scratch, offset, NtsProtocol.FieldNtsAuthenticator,
                BuildAuthenticatorBody(nonce, sealedData), 4 + PadTo4(nonce.Length) + PadTo4(sealedData.Length));
            return Slice(scratch, 0, offset);
        }

        private static byte[] BuildAuthenticatorBody(byte[] nonce, byte[] sealedData)
        {
            int nonceP = PadTo4(nonce.Length);
            int cipherP = PadTo4(sealedData.Length);
            byte[] body = new byte[4 + nonceP + cipherP];
            body[0] = (byte)(nonce.Length >> 8); body[1] = (byte)nonce.Length;
            body[2] = (byte)(sealedData.Length >> 8); body[3] = (byte)sealedData.Length;
            for (int i = 0; i < nonce.Length; i++) body[4 + i] = nonce[i];
            for (int i = 0; i < sealedData.Length; i++) body[4 + nonceP + i] = sealedData[i];
            return body;
        }

        private void HarvestCookies(byte[] encryptedEfs)
        {
            byte[][] fresh = new byte[8][];
            int count = 0;
            int offset = 0;
            while (offset + 4 <= encryptedEfs.Length && count < 8)
            {
                int type = NtsProtocol.ReadFieldType(encryptedEfs, offset);
                int len = NtsProtocol.ReadFieldLength(encryptedEfs, offset);
                if (len == 0) break;
                if (type == NtsProtocol.FieldNtsCookie)
                {
                    fresh[count] = Slice(encryptedEfs, offset + 4, len - 4);
                    count++;
                }
                offset += len;
            }
            if (count > 0)
            {
                _cookies = fresh;
                _cookieCount = count;
            }
        }

        private byte[] TakeCookie()
        {
            byte[] cookie = _cookies[0];
            for (int i = 1; i < _cookieCount; i++) _cookies[i - 1] = _cookies[i];
            _cookieCount--;
            return cookie;
        }

        private static int PadTo4(int n)
        {
            return (n + 3) & ~3;
        }

        private static byte[] Slice(byte[] source, int offset, int count)
        {
            byte[] result = new byte[count];
            for (int i = 0; i < count; i++) result[i] = source[offset + i];
            return result;
        }

        private static bool SameBytes(byte[] a, byte[] b)
        {
            if ((object)a == null || (object)b == null || a.Length != b.Length) return false;
            for (int i = 0; i < a.Length; i++)
            {
                if (a[i] != b[i]) return false;
            }
            return true;
        }
    }
}
