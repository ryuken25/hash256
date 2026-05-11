// Keccak-256 HASH token OpenCL kernel with full uint256 nonce support.
// nonce bytes = high_192_bits || counter64_be.

#define ROL64(a, n) (((a) << (n)) | ((a) >> (64 - (n))))

inline ulong bswap64(ulong v) {
    return ((v & 0xff00000000000000UL) >> 56) | ((v & 0x00ff000000000000UL) >> 40)
         | ((v & 0x0000ff0000000000UL) >> 24) | ((v & 0x000000ff00000000UL) >> 8)
         | ((v & 0x00000000ff000000UL) << 8)  | ((v & 0x0000000000ff0000UL) << 24)
         | ((v & 0x000000000000ff00UL) << 40) | ((v & 0x00000000000000ffUL) << 56);
}

__constant ulong RC[24] = {
  0x0000000000000001UL,0x0000000000008082UL,0x800000000000808aUL,0x8000000080008000UL,
  0x000000000000808bUL,0x0000000080000001UL,0x8000000080008081UL,0x8000000000008009UL,
  0x000000000000008aUL,0x0000000000000088UL,0x0000000080008009UL,0x000000008000000aUL,
  0x000000008000808bUL,0x800000000000008bUL,0x8000000000008089UL,0x8000000000008003UL,
  0x8000000000008002UL,0x8000000000000080UL,0x000000000000800aUL,0x800000008000000aUL,
  0x8000000080008081UL,0x8000000000008080UL,0x0000000080000001UL,0x8000000080008008UL
};

inline void keccak_f1600(ulong *s) {
  for (int r=0; r<24; r++) {
    ulong C0=s[0]^s[5]^s[10]^s[15]^s[20], C1=s[1]^s[6]^s[11]^s[16]^s[21], C2=s[2]^s[7]^s[12]^s[17]^s[22], C3=s[3]^s[8]^s[13]^s[18]^s[23], C4=s[4]^s[9]^s[14]^s[19]^s[24];
    ulong D0=C4^ROL64(C1,1), D1=C0^ROL64(C2,1), D2=C1^ROL64(C3,1), D3=C2^ROL64(C4,1), D4=C3^ROL64(C0,1);
    s[0]^=D0; s[5]^=D0; s[10]^=D0; s[15]^=D0; s[20]^=D0; s[1]^=D1; s[6]^=D1; s[11]^=D1; s[16]^=D1; s[21]^=D1; s[2]^=D2; s[7]^=D2; s[12]^=D2; s[17]^=D2; s[22]^=D2; s[3]^=D3; s[8]^=D3; s[13]^=D3; s[18]^=D3; s[23]^=D3; s[4]^=D4; s[9]^=D4; s[14]^=D4; s[19]^=D4; s[24]^=D4;
    ulong B00=s[0], B10=ROL64(s[1],1), B20=ROL64(s[2],62), B5=ROL64(s[3],28), B15=ROL64(s[4],27), B16=ROL64(s[5],36), B1=ROL64(s[6],44), B11=ROL64(s[7],6), B21=ROL64(s[8],55), B6=ROL64(s[9],20), B7=ROL64(s[10],3), B17=ROL64(s[11],10), B2=ROL64(s[12],43), B12=ROL64(s[13],25), B22=ROL64(s[14],39), B23=ROL64(s[15],41), B8=ROL64(s[16],45), B18=ROL64(s[17],15), B3=ROL64(s[18],21), B13=ROL64(s[19],8), B14=ROL64(s[20],18), B24=ROL64(s[21],2), B9=ROL64(s[22],61), B19=ROL64(s[23],56), B4=ROL64(s[24],14);
    s[0]=B00^((~B1)&B2); s[1]=B1^((~B2)&B3); s[2]=B2^((~B3)&B4); s[3]=B3^((~B4)&B00); s[4]=B4^((~B00)&B1);
    s[5]=B5^((~B6)&B7); s[6]=B6^((~B7)&B8); s[7]=B7^((~B8)&B9); s[8]=B8^((~B9)&B5); s[9]=B9^((~B5)&B6);
    s[10]=B10^((~B11)&B12); s[11]=B11^((~B12)&B13); s[12]=B12^((~B13)&B14); s[13]=B13^((~B14)&B10); s[14]=B14^((~B10)&B11);
    s[15]=B15^((~B16)&B17); s[16]=B16^((~B17)&B18); s[17]=B17^((~B18)&B19); s[18]=B18^((~B19)&B15); s[19]=B19^((~B15)&B16);
    s[20]=B20^((~B21)&B22); s[21]=B21^((~B22)&B23); s[22]=B22^((~B23)&B24); s[23]=B23^((~B24)&B20); s[24]=B24^((~B20)&B21);
    s[0]^=RC[r];
  }
}

__kernel void mine_keccak_u256(
    ulong c0, ulong c1, ulong c2, ulong c3,
    ulong p0, ulong p1, ulong p2,
    ulong d0, ulong d1, ulong d2, ulong d3,
    ulong counter_base,
    __global ulong* out_found_counter,
    __global int* out_found_flag) {
  ulong counter = counter_base + (ulong)get_global_id(0);
  ulong s[25];
  s[0]=c0; s[1]=c1; s[2]=c2; s[3]=c3;
  s[4]=p0; s[5]=p1; s[6]=p2; s[7]=bswap64(counter);
  s[8]=0x0000000000000001UL; s[9]=0; s[10]=0; s[11]=0; s[12]=0; s[13]=0; s[14]=0; s[15]=0; s[16]=0x8000000000000000UL;
  s[17]=0; s[18]=0; s[19]=0; s[20]=0; s[21]=0; s[22]=0; s[23]=0; s[24]=0;
  keccak_f1600(s);
  ulong h0=bswap64(s[0]); if (h0>d0) return;
  if (h0==d0) { ulong h1=bswap64(s[1]); if (h1>d1) return; if (h1==d1) { ulong h2=bswap64(s[2]); if (h2>d2) return; if (h2==d2) { ulong h3=bswap64(s[3]); if (h3>=d3) return; } } }
  if (atomic_cmpxchg(out_found_flag,0,1)==0) *out_found_counter=counter;
}

