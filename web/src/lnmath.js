// The routing-fee rule, one for all three sides (phone, LSP, here): a payment of N may cost at most
// 1 % of N in routing fees, at least 1 sat. Amounts are msat unless the name says sat.

export const routingFeeCapMsat = (amountMsat) => Math.max(Math.floor(amountMsat / 100), 1_000);

/** What the LSP keeps back for the routing fee of an offer payment (its own cap). */
export const offerReserveMsat = (amountMsat) => routingFeeCapMsat(amountMsat);

/** The most one payment can carry out of `availableMsat`: the largest whole-sat amount whose own fee
 *  cap still fits beside it (sat). Zero when not even 1 sat plus its 1 sat cap fits. */
export function sendAllSat(availableMsat) {
  // From 100 sat up the cap is 1 %, so amount * 1.01 must fit; below that the cap is its 1 sat minimum.
  const byPercent = Math.floor(availableMsat / 1_010);
  if (byPercent >= 100) return byPercent;
  return availableMsat >= 2_000 ? Math.floor((availableMsat - 1_000) / 1_000) : 0;
}
