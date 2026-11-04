# References

## Prior art (tools)
- **colour-science** (Python, BSD-3): reference implementations of CAM16, CAM16-UCS, ΔE variants and more. https://www.colour-science.org/ · https://github.com/colour-science/colour
- **darktable-lut-generator**: estimates a 3D LUT from RAW+OOC JPEG pairs (single camera) via LASSO on a trilinear LUT, with inverse-density sampling and a phone-display test pattern. License unverified. https://pypi.org/project/darktable-lut-generator/ · https://github.com/wilecoyote2015/darktabe_lut_generator
- **dtsolve**: fits darktable module parameters (AgX, color balance rgb, color equalizer, ...) to a look. Staged solving (tone first, then color, tints semi-manual), coordinate descent + random perturbation, CIELAB luma/chroma/hue error with heuristic weighting, RBF residual LUT. No license file (ideas only). https://jssfr.de/dtsolve/ · https://codeberg.org/jssfr/dtsolve
- Similar tools listed by darktable-lut-generator: https://github.com/bastibe/LUT-Maker · https://github.com/savuori/haldclut_dt
- **dnglab / rawler** (Rust RAW decoding). https://github.com/dnglab/dnglab
- **LibRaw** (LGPL-2.1 / CDDL-1.0). https://www.libraw.org/
- **darktable** (GPL-3). https://www.darktable.org/

## Color appearance
- Li, C., Li, Z., Wang, Z., Xu, Y., Luo, M. R., Cui, G., Melgosa, M., Brill, M. H., Pointer, M. (2017). *Comprehensive color solutions: CAM16, CAT16, and CAM16-UCS.* Color Research & Application, 42(6), 703–718. doi:10.1002/col.22131
- Luo, M. R., Cui, G., Li, C. (2006). *Uniform colour spaces based on CIECAM02 colour appearance model.* Color Research & Application, 31(4), 320–330. (UCS/LCD/SCD formulation)
- CIE 015 (Colorimetry); CIEDE2000 (Sharma, Wu, Dalal 2005 implementation notes).

## Crowdsourcing truth inference
- Zheng, Y., Li, G., Li, Y., Shan, C., Cheng, R. (2017). *Truth Inference in Crowdsourcing: Is the Problem Solved?* PVLDB 10(5). https://www.vldb.org/pvldb/vol10/p541-zheng.pdf
- Li, Q., et al. (2014). *Resolving Conflicts in Heterogeneous Data by Truth Discovery and Source Reliability Estimation* (CRH). SIGMOD.
- T-Crowd (ICDE 2018): unified categorical/continuous truth inference. https://dbgroup.cs.tsinghua.edu.cn/ligl/crowdsourcing/papers/icde_tcrowd.pdf
- Cai, D., et al. (2020). *Variational Bayesian Inference for Crowdsourcing Predictions.* https://arxiv.org/abs/2006.00778

## Poisoning-robust aggregation
- Cao, X., Fang, M., Liu, J., Gong, N. Z. (2021). *FLTrust: Byzantine-robust Federated Learning via Trust Bootstrapping.* NDSS. https://arxiv.org/abs/2012.13995
- Fung, C., Yoon, C. J. M., Beschastnikh, I. (2018/2020). *Mitigating Sybils in Federated Learning Poisoning* (FoolsGold). https://arxiv.org/abs/1808.04866 (considered and cut; see 05 §13)
- Yin, D., et al. (2018). *Byzantine-Robust Distributed Learning: Towards Optimal Statistical Rates* (coordinate-wise median / trimmed mean).
- Shejwalkar, V., Houmansadr, A. (2021). *Manipulating the Byzantine: Optimizing Model Poisoning Attacks and Defenses for Federated Learning.* NDSS.
- Sharma, A., et al. (2023). *FLAIR: Defense against Model Poisoning Attack in Federated Learning.* ASIACCS.
- Goldblum, M., et al. (2020). *Dataset Security for Machine Learning: Data Poisoning, Backdoor Attacks, and Defenses.* https://arxiv.org/abs/2012.10544
- W3C Web Authentication (WebAuthn) Level 2/3; `webauthn-rs`. https://www.w3.org/TR/webauthn-2/
- Koh, P. W., Liang, P. (2017). *Understanding Black-box Predictions via Influence Functions.* ICML. (considered; not used at hobby scale)
