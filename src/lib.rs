#![allow(type_alias_bounds)]
use ark_serialize::CanonicalSerialize;
use crate::hash_to_curve::htp_bls12381_g2;
use ark_ec::{AffineCurve, PairingEngine, bls12::G2Projective};
use ark_ff::{One, ToBytes, UniformRand};
use chacha20::cipher::{NewStreamCipher, SyncStreamCipher};
use chacha20::{ChaCha20, Key, Nonce};
use rand_core::RngCore;
use std::vec;

use log::error;
use thiserror::Error;

mod hash_to_curve;
pub mod key_generation;

type G1<P: ThresholdEncryptionParameters> = <P::E as PairingEngine>::G1Affine;
type G2<P: ThresholdEncryptionParameters> = <P::E as PairingEngine>::G2Affine;
type Fr<P: ThresholdEncryptionParameters> =  <<P::E as PairingEngine>::G1Affine as AffineCurve>::ScalarField;
type Fr2<P: ThresholdEncryptionParameters> =  <<P::E as PairingEngine>::G2Affine as AffineCurve>::ScalarField;

pub fn mock_hash<T: ark_serialize::CanonicalDeserialize>(message: &[u8]) -> T {
    let mut point_ser: Vec<u8> = Vec::new();
    let point = htp_bls12381_g2(message);
    println!("hash to curve res = {:?}", point);
    point.serialize(&mut point_ser).unwrap();
    T::deserialize(&point_ser[..]).unwrap()
}

pub trait ThresholdEncryptionParameters {
    type E: PairingEngine;
    // type H: HashToCurve<Output= G2<Self>>;
}

pub struct EncryptionPubkey<P: ThresholdEncryptionParameters> {
    pub key: G2<P>,
}

pub struct ShareVerificationPubkey<P: ThresholdEncryptionParameters> {
    pub decryptor_pubkeys: Vec<G2<P>>,
}

pub struct PrivkeyShare<P: ThresholdEncryptionParameters> {
    pub index: usize,
    pub privkey: Fr<P>,
    pub pubkey: G2<P>,
}

pub struct Ciphertext<P: ThresholdEncryptionParameters> {
    pub nonce: G1<P>, // U
    pub ciphertext: Vec<u8>, // V
    pub auth_tag: G2<P>, // W
}

pub struct DecryptionShare<P: ThresholdEncryptionParameters> {
    pub decryptor_index: usize, // i
    pub decryption_share: G1<P>, // U_i = x_i*U
}

#[derive(Debug, Error)]
/// Error type
pub enum ThresholdEncryptionError {
    /// Error
    #[error("ciphertext verification failed")]
    CiphertextVerificationFailed,

    /// Error
    #[error("Decryption share verification failed")]
    DecryptionShareVerificationFailed,

    /// Hashing to curve failed
    #[error("Could not hash to curve")]
    HashToCurveError,
    // Serialization error in Zexe
    // #[error(transparent)]
    // SerializationError(#[from] algebra::SerializationError),
}

/// Computes the ROM-heuristic hash `H(U, V, additional data) -> G2`,
/// used to construct the authentication tag for the ciphertext.
fn construct_tag_hash<P: ThresholdEncryptionParameters>(
    u: G1<P>,
    stream_ciphertext: &[u8],
    additional_data: &[u8],
) -> G2<P> {
    // Encode the data to be hashed as U || V || additional data
    // TODO: Length prefix V
    let mut hash_input = Vec::<u8>::new();
    u.write(&mut hash_input).unwrap();
    hash_input.extend_from_slice(stream_ciphertext);
    hash_input.extend_from_slice(additional_data);

    // let hasher = P::H::new().unwrap();
    // let domain = &b"auth_tag"[..];
    // let tag_hash = hasher.hash(domain, &hash_input).unwrap();
    // let tag_hash = mock_hash::<G2<P>>(/*&hash_input*/);
    let tag_hash = mock_hash(&hash_input);
    tag_hash
}

impl<P: ThresholdEncryptionParameters> EncryptionPubkey<P> {
    pub fn encrypt_msg<R: RngCore>(
        &self,
        msg: &[u8],
        additional_data: &[u8],
        rng: &mut R,
    ) -> Ciphertext<P> {
        // TODO: Come back and rename these
        let g1_generator = G1::<P>::prime_subgroup_generator();
        // let r = Fr::<P>::rand(rng);
        let r = Fr::<P>::one();
        let r2: Fr2::<P> = Fr2::<P>::one();
        
        let u = g1_generator.mul(r).into();

        println!("r = {:?}", r);
        println!("r2 = {:?}", r2);
        println!("g1_generator = {:?}", g1_generator);
        println!("u = {:?}", u);


        // Create the stream cipher key, which is r * Y
        // where r is the random nonce, and Y is the threshold pubkey that you are encrypting to.
        let stream_cipher_key_curve_elem = self.key.mul(r).into();

        // Convert this to stream cipher element into a key for the stream cipher
        // TODO: Use stream cipher Trait
        let mut prf_key = Vec::new();
        stream_cipher_key_curve_elem.write(&mut prf_key).unwrap();

        let prf_key_32 = hex::decode(sha256::digest_bytes(&prf_key)).expect("PRF key decoding failed");

        // This nonce doesn't matter, as we never have key re-use.
        // We keep it fixed to minimize the data transmitted.
        let chacha_nonce = Nonce::from_slice(b"secret nonce");
        let mut cipher = ChaCha20::new(Key::from_slice(&prf_key_32), chacha_nonce);

        // Encrypt the message
        let mut stream_ciphertext = msg.to_vec();
        cipher.apply_keystream(&mut stream_ciphertext);

        // Create the authentication tag
        // The authentication tag is r H(U, stream_ciphertext, additional_data)
        // So first we compute the tag hash, and then scale it by r to get the auth tag.
        let tag_hash = construct_tag_hash::<P>(u, &stream_ciphertext[..], additional_data);
        // let auth_tag = tag_hash.mul(r).into();
        let auth_tag = tag_hash.mul(r2).into();
        

        Ciphertext::<P> {
            nonce: u,
            ciphertext: stream_ciphertext,
            auth_tag,
        }
    }
}

impl<P: ThresholdEncryptionParameters> Ciphertext<P> {
    // TODO: Change this output to an enum
    /// Check that the provided ciphertext is validly constructed, and therefore is decryptable.
    pub fn check_ciphertext_validity(&self, additional_data: &[u8]) -> bool {
        // The authentication tag is valid iff e(nonce, tag hash) = e(g, auth tag)
        // Notice that this is equivalent to checking the following:
        // `e(nonce, tag hash) * e(g, auth tag)^{-1} = 1`
        // `e(nonce, tag hash) * e(-g, auth tag) = 1`
        // So first we construct the tag hash, and then we check whether this property holds or not.

        let tag_hash = construct_tag_hash::<P>(self.nonce, &self.ciphertext[..], additional_data);

        let g_inv = -G1::<P>::prime_subgroup_generator();


        let nonce_prep: <<P as ThresholdEncryptionParameters>::E as PairingEngine>::G1Prepared = self.nonce.into();
        let tag_hash_prep: <<P as ThresholdEncryptionParameters>::E as PairingEngine>::G2Prepared = tag_hash.into();
        let g_inv_prep: <<P as ThresholdEncryptionParameters>::E as PairingEngine>::G1Prepared = g_inv.into();
        let g_prep: <<P as ThresholdEncryptionParameters>::E as PairingEngine>::G1Prepared = G1::<P>::prime_subgroup_generator().into();
        let auth_tag_prep: <<P as ThresholdEncryptionParameters>::E as PairingEngine>::G2Prepared = self.auth_tag.into();
        
        // println!("====================================================");
        // println!("self.nonce.into() = {:?}", nonce_prep);
        // println!("====================================================");
        // println!("g_inv.into() = {:?}", g_inv_prep);
        // println!("====================================================");
        // println!("g.into() = {:?}", g_prep);

        // println!("====================================================");
        // println!("tag_hash = {:?}", tag_hash);
        // println!("====================================================");
        // println!("self.auth_tag = {:?}", self.auth_tag);
        // println!("====================================================");
        // println!("tag_hash.into() = {:?}", tag_hash_prep);
        // println!("====================================================");
        // println!("self.auth_tag.into() = {:?}", auth_tag_prep);

        // println!("====================================================");
        // println!("self.nonce = {:?}", self.nonce);
        // println!("====================================================");
        // println!("-g_inv = {:?}", -g_inv);
        // println!("====================================================");

        let pairing_prod_result = P::E::product_of_pairings(&[
            (self.nonce.into(), tag_hash.into()),
            (g_inv.into(), self.auth_tag.into()),
        ]);
        println!("pairing_prod_result = {:?}", pairing_prod_result);
        println!("====================================================");

        // Check that the result equals one
        let one = <<P as ThresholdEncryptionParameters>::E as PairingEngine>::Fqk::one();
        // println!("one = {:?}", one);

        // silly test
        // println!("self.nonce = {:?}", self.nonce);
        let test_pairing_prod_result = P::E::product_of_pairings(&[
            (
                self.nonce.into()
                ,
                tag_hash.into()
            ),
            (
                g_inv.into()
                ,
                // tag_hash.into()
                self.auth_tag.into()
            ),
        ]);

        // println!("test_pairing_prod_result = {:?}", test_pairing_prod_result);
        // println!("====================================================");
        // println!("TEST: {:?}", test_pairing_prod_result == one);


        println!("====================================================");
        println!("check_ciphertext_validity RETURNS: {:?}", pairing_prod_result == one);
        pairing_prod_result
            == one
    }
}

// TODO: Learn how rust crypto libraries handle private keys
impl<P: ThresholdEncryptionParameters> PrivkeyShare<P> {
    pub fn create_share(
        &self,
        c: &Ciphertext<P>,
        additional_data: &[u8],
    ) -> Result<DecryptionShare<P>, ThresholdEncryptionError> {
        let res = c.check_ciphertext_validity(additional_data);
        if res == false {
            return Err(ThresholdEncryptionError::CiphertextVerificationFailed);
        }
        let decryption_share = c.nonce.mul(self.privkey).into();
        Ok(DecryptionShare {
            decryptor_index: self.index,
            decryption_share,
        })
    }
}

impl<P: ThresholdEncryptionParameters> DecryptionShare<P> {
    pub fn verify_share(
        &self,
        c: &Ciphertext<P>,
        additional_data: &[u8],
        vpk: &ShareVerificationPubkey<P>,
    ) -> bool 
    {
        let res = c.check_ciphertext_validity(additional_data);
        if res == false {
            println!("verify_share: check_ciphertext_validity FAILED");
            return false;
        }

        // TODO: change pubkeys to G2, need to propagate changes in key_generation
        // e(Ui,H)=e(Yi,W)
        // let tag_hash = construct_tag_hash::<P>(c.nonce, &c.ciphertext[..], additional_data);
        // let pairing_prod_result = P::E::product_of_pairings(&[
        //     (
        //         self.decryption_share.into(),
        //         tag_hash.into(),
        //     ),
        //     (
        //         vpk.decryptor_pubkeys[self.decryptor_index].into(),
        //         c.auth_tag.into(),
        //     ),
        // ]);
        // pairing_prod_result
        //     == <<P as ThresholdEncryptionParameters>::E as PairingEngine>::Fqk::one()
        false
    }
}

pub fn share_combine<P: ThresholdEncryptionParameters>(
    plaintext: &mut [u8],
    c: Ciphertext<P>,
    additional_data: &[u8],
    shares: Vec<DecryptionShare<P>>,
) -> Result<(), ThresholdEncryptionError> {
    let res = c.check_ciphertext_validity(additional_data);
    if res == false {
        return Err(ThresholdEncryptionError::CiphertextVerificationFailed);
    }

    // let stream_cipher_key_curve_elem = ;/*Lagrange on shares here*/
    // let mut prf_key = Vec::new();
    // stream_cipher_key_curve_elem.write(&mut prf_key).unwrap();

    // let chacha_nonce = Nonce::from_slice(b"secret nonce");
    // let mut cipher = ChaCha20::new(Key::from_slice(&prf_key), chacha_nonce);

    // cipher.apply_keystream(&mut c.ciphertext);

    Ok(())
}


#[cfg(test)]
mod tests {

    use crate::key_generation::*;
    use crate::*;
    use ark_std::test_rng;

    // use algebra::curves::bls12::Bls12Parameters;// as algebra_bls12_params;
    // use ark_ec::bls12::Bls12Parameters;

    pub struct TestingParameters {}

    impl ThresholdEncryptionParameters for TestingParameters {
        type E = ark_bls12_381::Bls12_381;
        // type E = Bls12_377;
        // type H = bls_crypto::hash_to_curve::try_and_increment::TryAndIncrement::<bls_crypto::hashers::DirectHasher,
        // <ark_bls12_381::Parameters as Bls12Parameters>::G2Parameters
        // <algebra_bls12_params as Bls12Parameters>::G2Parameters
        // <Parameters as Bls12Parameters>::G2Parameters,
        // type H = Hasher;
    }

    #[test]
    fn completeness_test() {
        let mut rng = test_rng();
        let threshold = 3;
        let num_keys = 5;
        let (epk, svp, privkeys) = generate_keys::<TestingParameters, ark_std::rand::rngs::StdRng>(
            threshold, num_keys, &mut rng,
        );

        let msg: &[u8] = "abc".as_bytes();
        let ad: &[u8] = "".as_bytes();

        let ciphertext = epk.encrypt_msg(msg, ad, &mut rng);

        let mut dec_shares: Vec<DecryptionShare<TestingParameters>> = Vec::new();
        for i in 0..num_keys {
            dec_shares.push(privkeys[i].create_share(&ciphertext, ad).unwrap());
        }

        assert!(dec_shares[0].verify_share(&ciphertext, ad, &svp));
        assert!(dec_shares[1].verify_share(&ciphertext, ad, &svp));
        assert!(dec_shares[2].verify_share(&ciphertext, ad, &svp));
        assert!(dec_shares[3].verify_share(&ciphertext, ad, &svp));
    }
}
