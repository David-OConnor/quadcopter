#![allow(non_snake_case)]
#![allow(unused_parens)]
#![allow(non_camel_case_types)]
#[allow(non_upper_case_globals)]

//! Adapted from the official ELRS example here: https://github.com/ExpressLRS/ExpressLRS/blob/master/src/src/rx_main.cpp


///LUA///
const LUA_MAX_PARAMS: u32 =  32;
////

//// CONSTANTS ////
const SEND_LINK_STATS_TO_FC_INTERVAL: u8 = 100;
const DIVERSITY_ANTENNA_INTERVAL: u8 = 5;
const DIVERSITY_ANTENNA_RSSI_TRIGGER: u8 = 5;
const PACKET_TO_TOCK_SLACK: u8 = 200; // Desired buffer time between Packet ISR and Tock ISR
///////////////////

device_affinity_t ui_devices[] = {
#ifdef HAS_LED
  {&LED_device, 0},
#endif
  {&LUA_device,0},
#ifdef HAS_RGB
  {&RGB_device, 0},
#endif
#ifdef HAS_WIFI
  {&WIFI_device, 0},
#endif
#ifdef HAS_BUTTON
  {&Button_device, 0},
#endif
#ifdef HAS_VTX_SPI
  {&VTxSPI_device, 0},
#endif
};

static mut antenna: u8 = 0;    // which antenna is currently in use

hwTimer hwTimer;
POWERMGNT POWERMGNT;
PFD PFDloop;
GENERIC_CRC14 ota_crc(ELRS_CRC14_POLY);
ELRS_EEPROM eeprom;
RxConfig config;
Telemetry telemetry;

#ifdef PLATFORM_ESP8266
unsigned long rebootTime = 0;
extern bool webserverPreventAutoStart;
#endif

// #if defined(GPIO_PIN_PWM_OUTPUTS)
static mut SERVO_PINS: [u8; 69] = GPIO_PIN_PWM_OUTPUTS;
static mut SERVO_COUNT: [u8; 69] = ARRAY_SIZE(SERVO_PINS);
static mut Servo *Servos[SERVO_COUNT];
static mut newChannelsAvailable: bool = false;
// #endif

StubbornSender TelemetrySender(ELRS_TELEMETRY_MAX_PACKAGES);
static telemetryBurstCount: u8 = 0;
static telemetryBurstMax: u8 = 0;
// Maximum ms between LINK_STATISTICS packets for determining burst max
const TELEM_MIN_LINK_INTERVAL: u16 = 512;

StubbornReceiver MspReceiver(ELRS_MSP_MAX_PACKAGES);
const MspData: [u8; ELRS_MSP_BUFFER] = [0; ELRS_MSP_BUFFER];

static mut NextTelemetryType: u8 = ELRS_TELEMETRY_TYPE_LINK;
static mut telemBurstValid: bool = false;
/// Filters ////////////////
LPF LPF_Offset(2);
LPF LPF_OffsetDx(4);

// LPF LPF_UplinkRSSI(5);
LPF LPF_UplinkRSSI0(5);  // track rssi per antenna
LPF LPF_UplinkRSSI1(5);


/// LQ Calculation //////////
LQCALC<100> LQCalc;
uint8_t uplinkLQ;

const scanIndex: u8 = RATE_DEFAULT;
const ExpressLRS_nextAirRateIndex: u8 = 0;

// todo: Should these be consts, or static mut? I think the last is const?
const RawOffset: i32 = 0;
const prevRawOffset: i32 = 0;
const Offset: i32 = 0;
const  OffsetDx: i32 = 0;
const prevOffset: i32 = 0;
// RXtimerState_e RXtimerState;
const GotConnectionMillis: u32 = 0;
const ConsiderConnGoodMillis: u32 = 1000; // minimum time before we can consider a connection to be 'good'

///////////////////////////////////////////////

static mut NonceRX: u8 = 0; // nonce that we THINK we are up to.

static mut alreadyFHSS: bool = false;
static mut alreadyTLMresp: bool = false;

//////////////////////////////////////////////////////////////

///////Variables for Telemetry and Link Quality///////////////
uint32_t LastValidPacket = 0;           //Time the last valid packet was recv
uint32_t LastSyncPacket = 0;            //Time the last valid packet was recv

static uint32_t SendLinkStatstoFCintervalLastSent;
static uint8_t SendLinkStatstoFCForcedSends;

int16_t RFnoiseFloor; //measurement of the current RF noise floor
#if defined(DEBUG_RX_SCOREBOARD)
static bool lastPacketCrcError;
#endif
///////////////////////////////////////////////////////////////

/// Variables for Sync Behaviour ////
const cycleInterval: u32 = 0; // in ms
const RFmodeLastCycled: u32 = 0;
#define RFmodeCycleMultiplierSlow 10
const RFmodeCycleMultiplier: u8;
const LockRFmode: bool = false;
///////////////////////////////////////

// #if defined(DEBUG_BF_LINK_STATS)
// Debug vars
const debug1: u8 = 0;
const debug2: u8 = 0;
const debug3: u8 = 0;
const debug4: i8 = 0;
///////////////////////////////////////
// #endif

bool InBindingMode = false;

void reset_into_bootloader(void);
void EnterBindingMode();
void ExitBindingMode();
void UpdateModelMatch(uint8_t model);
void OnELRSBindMSP(uint8_t* packet);

bool ICACHE_RAM_ATTR IsArmed()
{
   return CRSF_to_BIT(crsf.GetChannelOutput(AUX1));
}

static uint8_t minLqForChaos()
{
    // Determine the most number of CRC-passing packets we could receive on
    // a single channel out of 100 packets that fill the LQcalc span.
    // The LQ must be GREATER THAN this value, not >=
    // The amount of time we coexist on the same channel is
    // 100 divided by the total number of packets in a FHSS loop (rounded up)
    // and there would be 4x packets received each time it passes by so
    // FHSShopInterval * ceil(100 / FHSShopInterval * numfhss) or
    // FHSShopInterval * trunc((100 + (FHSShopInterval * numfhss) - 1) / (FHSShopInterval * numfhss))
    // With a interval of 4 this works out to: 2.4=4, FCC915=4, AU915=8, EU868=8, EU/AU433=36
    const uint32_t numfhss = FHSSgetChannelCount();
    const uint8_t interval = ExpressLRS_currAirRate_Modparams->FHSShopInterval;
    return interval * ((interval * numfhss + 99) / (interval * numfhss));
}

void ICACHE_RAM_ATTR getRFlinkInfo()
{
    int32_t rssiDBM0 = LPF_UplinkRSSI0.SmoothDataINT;
    int32_t rssiDBM1 = LPF_UplinkRSSI1.SmoothDataINT;
    switch (antenna) {
        case 0:
            rssiDBM0 = LPF_UplinkRSSI0.update(Radio.LastPacketRSSI);
            break;
        case 1:
            rssiDBM1 = LPF_UplinkRSSI1.update(Radio.LastPacketRSSI);
            break;
    }

    int32_t rssiDBM = (antenna == 0) ? rssiDBM0 : rssiDBM1;
    crsf.PackedRCdataOut.ch15 = UINT10_to_CRSF(map(constrain(rssiDBM, ExpressLRS_currAirRate_RFperfParams->RXsensitivity, -50),
                                               ExpressLRS_currAirRate_RFperfParams->RXsensitivity, -50, 0, 1023));
    crsf.PackedRCdataOut.ch14 = UINT10_to_CRSF(fmap(uplinkLQ, 0, 100, 0, 1023));

    if (rssiDBM0 > 0) rssiDBM0 = 0;
    if (rssiDBM1 > 0) rssiDBM1 = 0;

    // BetaFlight/iNav expect positive values for -dBm (e.g. -80dBm -> sent as 80)
    crsf.LinkStatistics.uplink_RSSI_1 = -rssiDBM0;
    crsf.LinkStatistics.active_antenna = antenna;
    crsf.LinkStatistics.uplink_SNR = Radio.LastPacketSNR;
    //crsf.LinkStatistics.uplink_Link_quality = uplinkLQ; // handled in Tick
    crsf.LinkStatistics.rf_Mode = ExpressLRS_currAirRate_Modparams->enum_rate;
    //DBGLN(crsf.LinkStatistics.uplink_RSSI_1);
    #if defined(DEBUG_BF_LINK_STATS)
    crsf.LinkStatistics.downlink_RSSI = debug1;
    crsf.LinkStatistics.downlink_Link_quality = debug2;
    crsf.LinkStatistics.downlink_SNR = debug3;
    crsf.LinkStatistics.uplink_RSSI_2 = debug4;
    #else
    crsf.LinkStatistics.downlink_RSSI = 0;
    crsf.LinkStatistics.downlink_Link_quality = 0;
    crsf.LinkStatistics.downlink_SNR = 0;
    crsf.LinkStatistics.uplink_RSSI_2 = -rssiDBM1;
    #endif
}

void SetRFLinkRate(uint8_t index) // Set speed of RF link
{
    expresslrs_mod_settings_s *const ModParams = get_elrs_airRateConfig(index);
    expresslrs_rf_pref_params_s *const RFperf = get_elrs_RFperfParams(index);
    bool invertIQ = UID[5] & 0x01;

    hwTimer.updateInterval(ModParams->interval);
    Radio.Config(ModParams->bw, ModParams->sf, ModParams->cr, GetInitialFreq(),
                 ModParams->PreambleLen, invertIQ, ModParams->PayloadLength, 0
#if defined(RADIO_SX128X)
                 , uidMacSeedGet(), CRCInitializer, (ModParams->radio_type == RADIO_TYPE_SX128x_FLRC)
#endif
                 );

    // Wait for (11/10) 110% of time it takes to cycle through all freqs in FHSS table (in ms)
    cycleInterval = ((uint32_t)11U * FHSSgetChannelCount() * ModParams->FHSShopInterval * ModParams->interval) / (10U * 1000U);

    ExpressLRS_currAirRate_Modparams = ModParams;
    ExpressLRS_currAirRate_RFperfParams = RFperf;
    ExpressLRS_nextAirRateIndex = index; // presumably we just handled this
    telemBurstValid = false;
}

bool ICACHE_RAM_ATTR HandleFHSS()
{
    uint8_t modresultFHSS = (NonceRX + 1) % ExpressLRS_currAirRate_Modparams->FHSShopInterval;

    if ((ExpressLRS_currAirRate_Modparams->FHSShopInterval == 0) || alreadyFHSS == true || InBindingMode || (modresultFHSS != 0) || (connectionState == disconnected))
    {
        return false;
    }

    alreadyFHSS = true;
    Radio.SetFrequencyReg(FHSSgetNextFreq());

    uint8_t modresultTLM = (NonceRX + 1) % (TLMratioEnumToValue(ExpressLRS_currAirRate_Modparams->TLMinterval));

    if (modresultTLM != 0 || ExpressLRS_currAirRate_Modparams->TLMinterval == TLM_RATIO_NO_TLM) // if we are about to send a tlm response don't bother going back to rx
    {
        Radio.RXnb();
    }
    return true;
}

// ICACHE_RAM_ATTR
unsafe fn HandleSendTelemetryResponse() -> bool
{
    let mut data: [u8; 69] = [0; 69];
    let mut maxLength: u8 = 0;
    let mut packageIndex = 0;
    let modresult = (NonceRX + 1) % TLMratioEnumToValue(ExpressLRS_currAirRate_Modparams.TLMinterval);

    if ((connectionState == disconnected) || (ExpressLRS_currAirRate_Modparams.TLMinterval == TLM_RATIO_NO_TLM) || (alreadyTLMresp == true) || (modresult != 0))
    {
        return false; // don't bother sending tlm if disconnected or TLM is off
    }

if Regulatory_Domain_EU_CE_2400 {
    BeginClearChannelAssessment();
}

    alreadyTLMresp = true;
    Radio.TXdataBuffer[0] = TLM_PACKET;

    if (NextTelemetryType == ELRS_TELEMETRY_TYPE_LINK || !TelemetrySender.IsActive())
    {
        Radio.TXdataBuffer[1] = ELRS_TELEMETRY_TYPE_LINK;
        // The value in linkstatistics is "positivized" (inverted polarity)
        // and must be inverted on the TX side. Positive values are used
        // so save a bit to encode which antenna is in use
        Radio.TXdataBuffer[2] = crsf.LinkStatistics.uplink_RSSI_1 | (antenna << 7);
        Radio.TXdataBuffer[3] = crsf.LinkStatistics.uplink_RSSI_2 | (connectionHasModelMatch << 7);
        Radio.TXdataBuffer[4] = crsf.LinkStatistics.uplink_SNR;
        Radio.TXdataBuffer[5] = crsf.LinkStatistics.uplink_Link_quality;
        Radio.TXdataBuffer[6] = if MspReceiver.GetCurrentConfirm() { 1 } else { 0 };

        NextTelemetryType = ELRS_TELEMETRY_TYPE_DATA;
        // Start the count at 1 because the next will be DATA and doing +1 before checking
        // against Max below is for some reason 10 bytes more code
        telemetryBurstCount = 1;
    }
    else
    {
        if (telemetryBurstCount < telemetryBurstMax)
        {
            telemetryBurstCount++;
        }
        else
        {
            NextTelemetryType = ELRS_TELEMETRY_TYPE_LINK;
        }

        TelemetrySender.GetCurrentPayload(&packageIndex, &maxLength, &data);
        Radio.TXdataBuffer[1] = (packageIndex << ELRS_TELEMETRY_SHIFT) + ELRS_TELEMETRY_TYPE_DATA;
        Radio.TXdataBuffer[2] = maxLength > 0 ? *data : 0;
        Radio.TXdataBuffer[3] = maxLength >= 1 ? *(data + 1) : 0;
        Radio.TXdataBuffer[4] = maxLength >= 2 ? *(data + 2) : 0;
        Radio.TXdataBuffer[5] = maxLength >= 3 ? *(data + 3): 0;
        Radio.TXdataBuffer[6] = maxLength >= 4 ? *(data + 4): 0;
    }

    let crc: u16 = ota_crc.calc(Radio.TXdataBuffer, 7, CRCInitializer);
    Radio.TXdataBuffer[0] |= (crc >> 6) & 0b11111100;
    Radio.TXdataBuffer[7] = crc & 0xFF;

ifRegulatory_Domain_EU_CE_2400 {
if (ChannelIsClear())
}
    {
        Radio.TXnb();
    }
    return true;
}

// ICACHE_RAM_ATTR
fn HandleFreqCorr(value: bool)
{
    //DBGVLN(FreqCorrection);
    if (!value)
    {
        if (FreqCorrection < FreqCorrectionMax)
        {
            FreqCorrection += 1; //min freq step is ~ 61hz but don't forget we use FREQ_HZ_TO_REG_VAL so the units here are not hz!
        }
        else
        {
            FreqCorrection = FreqCorrectionMax;
            FreqCorrection = 0; //reset because something went wrong
            DBGLN("Max +FreqCorrection reached!");
        }
    }
    else
    {
        if (FreqCorrection > FreqCorrectionMin)
        {
            FreqCorrection -= 1; //min freq step is ~ 61hz
        }
        else
        {
            FreqCorrection = FreqCorrectionMin;
            FreqCorrection = 0; //reset because something went wrong
            DBGLN("Max -FreqCorrection reached!");
        }
    }
}

// ICACHE_RAM_ATTR
fn updatePhaseLock()
{
    if (connectionState != disconnected)
    {
        PFDloop.calcResult();
        PFDloop.reset();
        RawOffset = PFDloop.getResult();
        Offset = LPF_Offset.update(RawOffset);
        OffsetDx = LPF_OffsetDx.update(RawOffset - prevRawOffset);

        if (RXtimerState == tim_locked && LQCalc.currentIsSet())
        {
            if (NonceRX % 8 == 0) //limit rate of freq offset adjustment slightly
            {
                if (Offset > 0)
                {
                    hwTimer.incFreqOffset();
                }
                else if (Offset < 0)
                {
                    hwTimer.decFreqOffset();
                }
            }
        }

        if (connectionState != connected)
        {
            hwTimer.phaseShift(RawOffset >> 1);
        }
        else
        {
            hwTimer.phaseShift(Offset >> 2);
        }

        prevOffset = Offset;
        prevRawOffset = RawOffset;
    }

    DBGVLN("%d:%d:%d:%d:%d", Offset, RawOffset, OffsetDx, hwTimer.FreqOffset, uplinkLQ);
}

// ICACHE_RAM_ATTR
unsafe fn HWtimerCallbackTick() // this is 180 out of phase with the other callback, occurs mid-packet reception
{
    updatePhaseLock();
    NonceRX++;

    // if (!alreadyTLMresp && !alreadyFHSS && !LQCalc.currentIsSet()) // packet timeout AND didn't DIDN'T just hop or send TLM
    // {
    //     Radio.RXnb(); // put the radio cleanly back into RX in case of garbage data
    // }

    // Save the LQ value before the inc() reduces it by 1
    uplinkLQ = LQCalc.getLQ();
    crsf.LinkStatistics.uplink_Link_quality = uplinkLQ;
    // Only advance the LQI period counter if we didn't send Telemetry this period
    if (!alreadyTLMresp) {
        LQCalc.inc();
    }

    alreadyTLMresp = false;
    alreadyFHSS = false;
    crsf.RXhandleUARTout();
}

//////////////////////////////////////////////////////////////
// flip to the other antenna
// no-op if GPIO_PIN_ANTENNA_SELECT not defined
#[inline(always)]
fn switchAntenna()
{
if GPIO_PIN_ANTENNA_SELECT && USE_DIVERSITY {
    if(config.GetAntennaMode() == 2){
    //0 and 1 is use for gpio_antenna_select
    // 2 is diversity
        antenna = !antenna;
        (antenna == 0) ? LPF_UplinkRSSI0.reset() : LPF_UplinkRSSI1.reset(); // discard the outdated value after switching
        digitalWrite(GPIO_PIN_ANTENNA_SELECT, antenna);
    }
}
}

// ICACHE_RAM_ATTR
fn updateDiversity()
{

#if defined(GPIO_PIN_ANTENNA_SELECT) && defined(USE_DIVERSITY)
    if(config.GetAntennaMode() == 2){
    //0 and 1 is use for gpio_antenna_select
    // 2 is diversity
        static int32_t prevRSSI;        // saved rssi so that we can compare if switching made things better or worse
        static int32_t antennaLQDropTrigger;
        static int32_t antennaRSSIDropTrigger;
        int32_t rssi = (antenna == 0) ? LPF_UplinkRSSI0.SmoothDataINT : LPF_UplinkRSSI1.SmoothDataINT;
        int32_t otherRSSI = (antenna == 0) ? LPF_UplinkRSSI1.SmoothDataINT : LPF_UplinkRSSI0.SmoothDataINT;

        //if rssi dropped by the amount of DIVERSITY_ANTENNA_RSSI_TRIGGER
        if ((rssi < (prevRSSI - DIVERSITY_ANTENNA_RSSI_TRIGGER)) && antennaRSSIDropTrigger >= DIVERSITY_ANTENNA_INTERVAL)
        {
            switchAntenna();
            antennaLQDropTrigger = 1;
            antennaRSSIDropTrigger = 0;
        }
        else if (rssi > prevRSSI || antennaRSSIDropTrigger < DIVERSITY_ANTENNA_INTERVAL)
        {
            prevRSSI = rssi;
            antennaRSSIDropTrigger++;
        }

        // if we didn't get a packet switch the antenna
        if (!LQCalc.currentIsSet() && antennaLQDropTrigger == 0)
        {
            switchAntenna();
            antennaLQDropTrigger = 1;
            antennaRSSIDropTrigger = 0;
        }
        else if (antennaLQDropTrigger >= DIVERSITY_ANTENNA_INTERVAL)
        {
            // We switched antenna on the previous packet, so we now have relatively fresh rssi info for both antennas.
            // We can compare the rssi values and see if we made things better or worse when we switched
            if (rssi < otherRSSI)
            {
                // things got worse when we switched, so change back.
                switchAntenna();
                antennaLQDropTrigger = 1;
                antennaRSSIDropTrigger = 0;
            }
            else
            {
                // all good, we can stay on the current antenna. Clear the flag.
                antennaLQDropTrigger = 0;
            }
        }
        else if (antennaLQDropTrigger > 0)
        {
            antennaLQDropTrigger ++;
        }
    }else {
        digitalWrite(GPIO_PIN_ANTENNA_SELECT, config.GetAntennaMode());
        antenna = config.GetAntennaMode();
    }
#endif
}

// ICACHE_RAM_ATTR
fn HWtimerCallbackTock()
{
if Regulatory_Domain_EU_CE_2400 {
    // Emulate that TX just happened, even if it didn't because channel is not clear
    if (!LBTSuccessCalc.currentIsSet())
    {
        Radio.TXdoneCallback();
    }
}

    PFDloop.intEvent(micros()); // our internal osc just fired

    updateDiversity();
    let didFHSS: bool = HandleFHSS();
    let tlmSent: bool = HandleSendTelemetryResponse();

    if DEBUG_RX_SCOREBOARD {
        let mut lastPacketWasTelemetry = false;
        if (!LQCalc.currentIsSet() && !lastPacketWasTelemetry)
        DBGW(lastPacketCrcError? '.': '_');
        lastPacketCrcError = false;
        lastPacketWasTelemetry = tlmSent;
    }
}

fn LostConnection()
{
    DBGLN("lost conn fc=%d fo=%d", FreqCorrection, hwTimer.FreqOffset);

    RFmodeCycleMultiplier = 1;
    connectionState = disconnected; //set lost connection
    RXtimerState = tim_disconnected;
    hwTimer.resetFreqOffset();
    FreqCorrection = 0;
    #if defined(RADIO_SX127X)
    Radio.SetPPMoffsetReg(0);
    #endif
    Offset = 0;
    OffsetDx = 0;
    RawOffset = 0;
    prevOffset = 0;
    GotConnectionMillis = 0;
    uplinkLQ = 0;
    LQCalc.reset();
    LPF_Offset.init(0);
    LPF_OffsetDx.init(0);
    alreadyTLMresp = false;
    alreadyFHSS = false;

    if (!InBindingMode)
    {
        while(micros() - PFDloop.getIntEventTime() > 250); // time it just after the tock()
        hwTimer.stop();
        SetRFLinkRate(ExpressLRS_nextAirRateIndex); // also sets to initialFreq
        Radio.RXnb();
    }
}

// ICACHE_RAM_ATTR
fn TentativeConnection(now: u64)
{
    PFDloop.reset();
    connectionState = tentative;
    connectionHasModelMatch = false;
    RXtimerState = tim_disconnected;
    DBGLN("tentative conn");
    FreqCorrection = 0;
    Offset = 0;
    prevOffset = 0;
    LPF_Offset.init(0);
    RFmodeLastCycled = now; // give another 3 sec for lock to occur

    // The caller MUST call hwTimer.resume(). It is not done here because
    // the timer ISR will fire immediately and preempt any other code
}

fn GotConnection(now: u64)
{
    if (connectionState == connected)
    {
        return; // Already connected
    }

if LOCK_ON_FIRST_CONNECTION {
    LockRFmode = true;
}

    connectionState = connected; //we got a packet, therefore no lost connection
    RXtimerState = tim_tentative;
    GotConnectionMillis = now;

    DBGLN("got conn");
}

// ICACHE_RAM_ATTR
fn ProcessRfPacket_RC()
{
    // Must be fully connected to process RC packets, prevents processing RC
    // during sync, where packets can be received before connection
    if (connectionState != connected) {
        return;
    }

    let telemetryConfirmValue: bool = UnpackChannelData(Radio.RXdataBuffer, &crsf,
        NonceRX, TLMratioEnumToValue(ExpressLRS_currAirRate_Modparams->TLMinterval));
    TelemetrySender.ConfirmCurrentPayload(telemetryConfirmValue);

    // No channels packets to the FC if no model match
    if (connectionHasModelMatch)
    {
        if GPIO_PIN_PWM_OUTPUTS {
            newChannelsAvailable = true;
        } else {
            crsf.sendRCFrameToFC();
        }
    }
}

/**
 * Process the assembled MSP packet in MspData[]
 **/
// ICACHE_RAM_ATTR
fn MspReceiveComplete()
{
    if (MspData[7] == MSP_SET_RX_CONFIG && MspData[8] == MSP_ELRS_MODEL_ID)
    {
        UpdateModelMatch(MspData[9]);
    }
    else if (MspData[0] == MSP_ELRS_SET_RX_WIFI_MODE)
    {

    }
if HAS_VTX_SPI {
    else if (MspData[7] == MSP_SET_VTX_CONFIG)
    {
        vtxSPIBandChannelIdx = MspData[8];
        if (MspData[6] >= 4) // If packet has 4 bytes it also contains power idx and pitmode.
        {
            vtxSPIPowerIdx = MspData[10];
            vtxSPIPitmode = MspData[11];
        }
        devicesTriggerEvent();
    }
}
    else
    {
        // No MSP data to the FC if no model match
        if (connectionHasModelMatch)
        {
            crsf_ext_header_t *receivedHeader = (crsf_ext_header_t *) MspData;
            if ((receivedHeader->dest_addr == CRSF_ADDRESS_BROADCAST || receivedHeader->dest_addr == CRSF_ADDRESS_FLIGHT_CONTROLLER))
            {
                crsf.sendMSPFrameToFC(MspData);
            }

            if ((receivedHeader->dest_addr == CRSF_ADDRESS_BROADCAST || receivedHeader->dest_addr == CRSF_ADDRESS_CRSF_RECEIVER))
            {
                crsf.ParameterUpdateData[0] = MspData[CRSF_TELEMETRY_TYPE_INDEX];
                crsf.ParameterUpdateData[1] = MspData[CRSF_TELEMETRY_FIELD_ID_INDEX];
                crsf.ParameterUpdateData[2] = MspData[CRSF_TELEMETRY_FIELD_CHUNK_INDEX];
                luaParamUpdateReq();
            }
        }
    }

    MspReceiver.Unlock();
}

// ICACHE_RAM_ATTR
fn ProcessRfPacket_MSP()
{
    // Always examine MSP packets for bind information if in bind mode
    // [1] is the package index, first packet of the MSP
    if (InBindingMode && Radio.RXdataBuffer[1] == 1 && Radio.RXdataBuffer[2] == MSP_ELRS_BIND)
    {
        OnELRSBindMSP((uint8_t *)&Radio.RXdataBuffer[2]);
        return;
    }

    // Must be fully connected to process MSP, prevents processing MSP
    // during sync, where packets can be received before connection
    if (connectionState != connected) {
        return;
    }

    let currentMspConfirmValue: bool = MspReceiver.GetCurrentConfirm();
    MspReceiver.ReceiveData(Radio.RXdataBuffer[1], Radio.RXdataBuffer + 2);
    if (currentMspConfirmValue != MspReceiver.GetCurrentConfirm())
    {
        NextTelemetryType = ELRS_TELEMETRY_TYPE_LINK;
    }
    if (MspReceiver.HasFinishedData())
    {
        MspReceiveComplete();
    }
}

// ICACHE_RAM_ATTR
fn  ProcessRfPacket_SYNC(now: u64) -> bool
{
    // Verify the first two of three bytes of the binding ID, which should always match
    if (Radio.RXdataBuffer[4] != UID[3] || Radio.RXdataBuffer[5] != UID[4]) {
        return false;
    }

    // The third byte will be XORed with inverse of the ModelId if ModelMatch is on
    // Only require the first 18 bits of the UID to match to establish a connection
    // but the last 6 bits must modelmatch before sending any data to the FC
    if ((Radio.RXdataBuffer[6] & ~MODELMATCH_MASK) != (UID[5] & ~MODELMATCH_MASK)) {
        return false;
    }

    LastSyncPacket = now;
    if DEBUG_RX_SCOREBOARD {
    DBGW('s');
}

    // Will change the packet air rate in loop() if this changes
    ExpressLRS_nextAirRateIndex = (Radio.RXdataBuffer[3] >> SYNC_PACKET_RATE_OFFSET) & SYNC_PACKET_RATE_MASK;
    // Update switch mode encoding immediately
    OtaSetSwitchMode((OtaSwitchMode_e)((Radio.RXdataBuffer[3] >> SYNC_PACKET_SWITCH_OFFSET) & SYNC_PACKET_SWITCH_MASK));
    // Update TLM ratio
   let TLMrateIn: TlmRatio = (expresslrs_tlm_ratio_e)((Radio.RXdataBuffer[3] >> SYNC_PACKET_TLM_OFFSET) & SYNC_PACKET_TLM_MASK);
    if (ExpressLRS_currAirRate_Modparams.TLMinterval != TLMrateIn)
    {
        DBGLN("New TLMrate: %d", TLMrateIn);
        ExpressLRS_currAirRate_Modparams.TLMinterval = TLMrateIn;
        telemBurstValid = false;
    }

    // modelId = 0xff indicates modelMatch is disabled, the XOR does nothing in that case
    uint8_t modelXor = (~config.GetModelId()) & MODELMATCH_MASK;
    bool modelMatched = Radio.RXdataBuffer[6] == (UID[5] ^ modelXor);
    DBGVLN("MM %u=%u %d", Radio.RXdataBuffer[6], UID[5], modelMatched);

    if (connectionState == disconnected
        || NonceRX != Radio.RXdataBuffer[2]
        || FHSSgetCurrIndex() != Radio.RXdataBuffer[1]
        || connectionHasModelMatch != modelMatched)
    {
        //DBGLN("\r\n%ux%ux%u", NonceRX, Radio.RXdataBuffer[2], Radio.RXdataBuffer[1]);
        FHSSsetCurrIndex(Radio.RXdataBuffer[1]);
        NonceRX = Radio.RXdataBuffer[2];
        TentativeConnection(now);
        // connectionHasModelMatch must come after TentativeConnection, which resets it
        connectionHasModelMatch = modelMatched;
        return true;
    }

    return false;
}

// ICACHE_RAM_ATTR
fn ProcessRFPacket(SX12xxDriverCommon::rx_status const status)
{
    if (status != SX12xxDriverCommon::SX12XX_RX_OK)
    {
        DBGVLN("HW CRC error");
        # if defined(DEBUG_RX_SCOREBOARD)
        lastPacketCrcError = true;
        # endif
        return;
    }
    let beginProcessing = micros();
    let inCRC: u16 = (((uint16_t)(Radio.RXdataBuffer[0] & 0b11111100)) << 6) | Radio.RXdataBuffer[7];
    let type_: u8 = Radio.RXdataBuffer[0] & 0b11;

    // For smHybrid the CRC only has the packet type in byte 0
    // For smHybridWide the FHSS slot is added to the CRC in byte 0 on RC_DATA_PACKETs
    if (type_ != RC_DATA_PACKET || OtaSwitchModeCurrent != smHybridWide)
    {
        Radio.RXdataBuffer[0] = type_;
    } else {
        let NonceFHSSresult: u8 = NonceRX % ExpressLRS_currAirRate_Modparams -> FHSShopInterval;
        Radio.RXdataBuffer[0] = type_ | (NonceFHSSresult << 2);
    }
    let calculatedCRC: u16 = ota_crc.calc(Radio.RXdataBuffer, 7, CRCInitializer);

    if (inCRC != calculatedCRC)
    {
        DBGV("CRC error: ");
        for i in 0..8 {
            {
                DBGV("%x,", Radio.RXdataBuffer[i]);
            }
            DBGVCR;
            if DEBUG_RX_SCOREBOARD {
                lastPacketCrcError = true;
            }
            return;
        }
        PFDloop.extEvent(beginProcessing + PACKET_TO_TOCK_SLACK);

        let mut doStartTimer = false;
        let now = millis();

        LastValidPacket = now;

        match type_ {
        RC_DATA_PACKET => { // Standard RC Data  Packet
            ProcessRfPacket_RC();
        }
        MSP_DATA_PACKET => {
            ProcessRfPacket_MSP();
        }
    TLM_PACKET => { //telemetry packet from master
    // not implimented yet
}
    SYNC_PACKET => { //sync packet from master
        doStartTimer = ProcessRfPacket_SYNC(now) && !InBindingMode;
}
    _ => ()
    }

    // Store the LQ/RSSI/Antenna
    getRFlinkInfo();
    // Received a packet, that's the definition of LQ
    LQCalc.add();
    // Extend sync duration since we've received a packet at this rate
    // but do not extend it indefinitely
    RFmodeCycleMultiplier = RFmodeCycleMultiplierSlow;

if DEBUG_RX_SCOREBOARD {
    if ( type_ != SYNC_PACKET) DBGW(connectionHasModelMatch? 'R': 'r');
}
    if (doStartTimer)
        hwTimer.resume(); // will throw an interrupt immediately
}

    // ICACHE_RAM_ATTR
fn RXdoneISR(status: SX12xxDriverCommon::rx_status )
{
    ProcessRFPacket(status);
}

// ICACHE_RAM_ATTR
fn  TXdoneISR()
{
    Radio.RXnb();
#if defined(DEBUG_RX_SCOREBOARD)
    DBGW('T');
#endif
}


fn setupConfigAndPocCheck()
{
    eeprom.Begin();
    config.SetStorageProvider(&eeprom); // Pass pointer to the Config class for access to storage
    config.Load();

    DBGLN("ModelId=%u", config.GetModelId());

#ifndef MY_UID
    // Increment the power on counter in eeprom
    config.SetPowerOnCounter(config.GetPowerOnCounter() + 1);
    config.Commit();

    // If we haven't reached our binding mode power cycles
    // and we've been powered on for 2s, reset the power on counter
    if (config.GetPowerOnCounter() < 3)
    {
        delay(2000);
        config.SetPowerOnCounter(0);
        config.Commit();
    }
#endif
}

fn setupTarget() {
    if GPIO_PIN_ANTENNA_SELECT {
        pinMode(GPIO_PIN_ANTENNA_SELECT, OUTPUT);
        digitalWrite(GPIO_PIN_ANTENNA_SELECT, LOW);
    }

    setupTargetCommon();
}

fn setupBindingFromConfig()
{
// Use the user defined binding phase if set,
// otherwise use the bind flag and UID in eeprom for UID
if MY_UID {
    // Check the byte that indicates if RX has been bound
    if (config.GetIsBound())
    {
        DBGLN("RX has been bound previously, reading the UID from eeprom...");
        const uint8_t* storedUID = config.GetUID();
    for i in 0..UID_LEN {
        {
            UID[i] = storedUID[i];
        }
        DBGLN("UID = %d, %d, %d, %d, %d, %d", UID[0], UID[1], UID[2], UID[3], UID[4], UID[5]);
        CRCInitializer = (UID[4] << 8) | UID[5];
    }
}
}

fn setupRadio()
{
    Radio.currFreq = GetInitialFreq();

    bool init_success = Radio.Begin();
    POWERMGNT.init();
    if (!init_success)
    {
        DBGLN("Failed to detect RF chipset!!!");
        connectionState = radioFailed;
        return;
    }

    POWERMGNT.setPower((PowerLevels_e)config.GetPower());

    if Regulatory_Domain_EU_CE_2400) {
        LBTEnabled = (MaxPower > PWR_10mW);
    }

    Radio.RXdoneCallback = &RXdoneISR;
    Radio.TXdoneCallback = &TXdoneISR;

    SetRFLinkRate(RATE_DEFAULT);
    RFmodeCycleMultiplier = 1;
}

fn updateTelemetryBurst()
{
    if (telemBurstValid)
        return;
    telemBurstValid = true;

    let hz: u32 = RateEnumToHz(ExpressLRS_currAirRate_Modparams.enum_rate);
    let ratiodiv: u32 = TLMratioEnumToValue(ExpressLRS_currAirRate_Modparams.TLMinterval);
    // telemInterval = 1000 / (hz / ratiodiv);
    // burst = TELEM_MIN_LINK_INTERVAL / telemInterval;
    // This ^^^ rearranged to preserve precision vvv
    telemetryBurstMax = TELEM_MIN_LINK_INTERVAL * hz / ratiodiv / 1000U;

    // Reserve one slot for LINK telemetry
    if (telemetryBurstMax > 1)
        --telemetryBurstMax;
    else
        telemetryBurstMax = 1;
    //DBGLN("TLMburst: %d", telemetryBurstMax);

    // Notify the sender to adjust its expected throughput
    TelemetrySender.UpdateTelemetryRate(hz, ratiodiv, telemetryBurstMax);
}

/* If not connected will rotate through the RF modes looking for sync
 * and blink LED
 */
fn cycleRfMode(now: u64)
{
    if (connectionState == connected || connectionState == wifiUpdate || InBindingMode)
        return;

    // Actually cycle the RF mode if not LOCK_ON_FIRST_CONNECTION
    if (LockRFmode == false && (now - RFmodeLastCycled) > (cycleInterval * RFmodeCycleMultiplier))
    {
        RFmodeLastCycled = now;
        LastSyncPacket = now;           // reset this variable
        SendLinkStatstoFCForcedSends = 2;
        SetRFLinkRate(scanIndex % RATE_MAX); // switch between rates
        LQCalc.reset();
        // Display the current air rate to the user as an indicator something is happening
        scanIndex++;
        Radio.RXnb();
        INFOLN("%u", ExpressLRS_currAirRate_Modparams->interval);

        // Switch to FAST_SYNC if not already in it (won't be if was just connected)
        RFmodeCycleMultiplier = 1;
    } // if time to switch RF mode
}

fn servosUpdate(now: u64) {
if GPIO_PIN_PWM_OUTPUTS {
    // The ESP waveform generator is nice because it doesn't change the value
    // mid-cycle, but it does busywait if there's already a change queued.
    // Updating every 20ms minimizes the amount of waiting (0-800us cycling
    // after it syncs up) where 19ms always gets a 1000-1800us wait cycling
    let mut lastUpdate: u32 = 0;
    let elapsed = now - lastUpdate;
    if (elapsed < 20)
        return;

    if (newChannelsAvailable)
    {
        newChannelsAvailable = false;
        for ch in 0..SERVO_COUNT {
        {
            const rx_config_pwm_t *chConfig = config.GetPwmChannel(ch);
            uint16_t us = CRSF_to_US(crsf.GetChannelOutput(chConfig->val.inputChannel));
            if (chConfig.val.inverted)
                us = 3000 - us;

            if (Servos[ch])
                Servos[ch]->writeMicroseconds(us);
            else if (us >= 988 && us <= 2012)
            {
                // us might be out of bounds if this is a switch channel and it has not been
                // received yet. Delay initializing the servo until the channel is valid
                Servo *servo = new Servo();
                Servos[ch] = servo;
                servo->attach(SERVO_PINS[ch], 988, 2012, us);
            }
        } /* for each servo */
    } /* if newChannelsAvailable */

    else if (elapsed > 1000U && connectionState == connected)
    {
        // No update for 1s, go to failsafe
    for i in 0..SERVO_COUNT {
        {
            // Note: Failsafe values do not respect the inverted flag, failsafes are absolute
            let us: u16 = config.GetPwmChannel(ch).val.failsafe + 988;
            if (Servos[ch])
                Servos[ch]->writeMicroseconds(us);
        }
    }

    else
        return; // prevent updating lastUpdate

    // need to sample actual millis at the end to account for any
    // waiting that happened in Servo::writeMicroseconds()
    lastUpdate = millis();
}
}

fn updateBindingMode()
{
    // If the eeprom is indicating that we're not bound
    // and we're not already in binding mode, enter binding
    if (!config.GetIsBound() && !InBindingMode)
    {
        INFOLN("RX has not been bound, enter binding mode...");
        EnterBindingMode();
    }
    // If in binding mode and the bind packet has come in, leave binding mode
    else if (config.GetIsBound() && InBindingMode)
    {
        ExitBindingMode();
    }

// #ifndef MY_UID
    // If the power on counter is >=3, enter binding and clear counter
    if (config.GetPowerOnCounter() >= 3)
    {
        config.SetPowerOnCounter(0);
        config.Commit();

        INFOLN("Power on counter >=3, enter binding mode...");
        EnterBindingMode();
    }
// #endif
}

fn checkSendLinkStatsToFc(uint32_t now)
{
    if (now - SendLinkStatstoFCintervalLastSent > SEND_LINK_STATS_TO_FC_INTERVAL)
    {
        if (connectionState == disconnected)
        {
            getRFlinkInfo();
        }

        if ((connectionState != disconnected && connectionHasModelMatch) ||
            SendLinkStatstoFCForcedSends)
        {
            crsf.sendLinkStatisticsToFC();
            SendLinkStatstoFCintervalLastSent = now;
            if (SendLinkStatstoFCForcedSends)
                --SendLinkStatstoFCForcedSends;
        }
    }
}

fn setup()
{
    setupTarget();

    // Init EEPROM and load config, checking powerup count
    setupConfigAndPocCheck();

    INFOLN("ExpressLRS Module Booting...");

    devicesRegister(ui_devices, ARRAY_SIZE(ui_devices));
    devicesInit();

    setupBindingFromConfig();

    FHSSrandomiseFHSSsequence(uidMacSeedGet());

    setupRadio();

    if (connectionState != radioFailed)
    {
        // RFnoiseFloor = MeasureNoiseFloor(); //TODO move MeasureNoiseFloor to driver libs
        // DBGLN("RF noise floor: %d dBm", RFnoiseFloor);

        hwTimer.callbackTock = &HWtimerCallbackTock;
        hwTimer.callbackTick = &HWtimerCallbackTick;

        MspReceiver.SetDataToReceive(ELRS_MSP_BUFFER, MspData, ELRS_MSP_BYTES_PER_CALL);
        Radio.RXnb();
        crsf.Begin();
        hwTimer.init();
    }

    devicesStart();
}

fn loop_()
{
    unsigned long now = millis();
    HandleUARTin();
    if (hwTimer.running == false)
    {
        crsf.RXhandleUARTout();
    }

    devicesUpdate(now);

    #if defined(PLATFORM_ESP8266) && defined(AUTO_WIFI_ON_INTERVAL)
    // If the reboot time is set and the current time is past the reboot time then reboot.
    if (rebootTime != 0 && now > rebootTime) {
        ESP.restart();
    }
    #endif

    if (config.IsModified() && !InBindingMode)
    {
        Radio.SetTxIdleMode();
        LostConnection();
        config.Commit();
        devicesTriggerEvent();
    }

    if (connectionState > MODE_STATES)
    {
        return;
    }

    if ((connectionState != disconnected) && (ExpressLRS_currAirRate_Modparams->index != ExpressLRS_nextAirRateIndex)){ // forced change
        DBGLN("Req air rate change %u->%u", ExpressLRS_currAirRate_Modparams->index, ExpressLRS_nextAirRateIndex);
        LostConnection();
        LastSyncPacket = now;           // reset this variable to stop rf mode switching and add extra time
        RFmodeLastCycled = now;         // reset this variable to stop rf mode switching and add extra time
        SendLinkStatstoFCintervalLastSent = 0;
        SendLinkStatstoFCForcedSends = 2;
    }

    if (connectionState == tentative && (now - LastSyncPacket > ExpressLRS_currAirRate_RFperfParams->RxLockTimeoutMs))
    {
        DBGLN("Bad sync, aborting");
        LostConnection();
        RFmodeLastCycled = now;
        LastSyncPacket = now;
    }

    cycleRfMode(now);
    servosUpdate(now);

    uint32_t localLastValidPacket = LastValidPacket; // Required to prevent race condition due to LastValidPacket getting updated from ISR
    if ((connectionState == disconnectPending) ||
        ((connectionState == connected) && ((int32_t)ExpressLRS_currAirRate_RFperfParams->DisconnectTimeoutMs < (int32_t)(now - localLastValidPacket)))) // check if we lost conn.
    {
        LostConnection();
    }

    if ((connectionState == tentative) && (abs(OffsetDx) <= 10) && (Offset < 100) && (LQCalc.getLQRaw() > minLqForChaos())) //detects when we are connected
    {
        GotConnection(now);
    }

    checkSendLinkStatsToFc(now);

    if ((RXtimerState == tim_tentative) && ((now - GotConnectionMillis) > ConsiderConnGoodMillis) && (abs(OffsetDx) <= 5))
    {
        RXtimerState = tim_locked;
        DBGLN("Timer locked");
    }

    uint8_t *nextPayload = 0;
    uint8_t nextPlayloadSize = 0;
    if (!TelemetrySender.IsActive() && telemetry.GetNextPayload(&nextPlayloadSize, &nextPayload))
    {
        TelemetrySender.SetDataToTransmit(nextPlayloadSize, nextPayload, ELRS_TELEMETRY_BYTES_PER_CALL);
    }
    updateTelemetryBurst();
    updateBindingMode();
}

void EnterBindingMode()
{
    if ((connectionState == connected) || InBindingMode) {
        // Don't enter binding if:
        // - we're already connected
        // - we're already binding
        DBGLN("Cannot enter binding mode!");
        return;
    }

    // Set UID to special binding values
    UID[0] = BindingUID[0];
    UID[1] = BindingUID[1];
    UID[2] = BindingUID[2];
    UID[3] = BindingUID[3];
    UID[4] = BindingUID[4];
    UID[5] = BindingUID[5];

    CRCInitializer = 0;
    config.SetIsBound(false);
    InBindingMode = true;

    // Start attempting to bind
    // Lock the RF rate and freq while binding
    SetRFLinkRate(RATE_BINDING);
    Radio.SetFrequencyReg(GetInitialFreq());
    // If the Radio Params (including InvertIQ) parameter changed, need to restart RX to take effect
    Radio.RXnb();

    DBGLN("Entered binding mode at freq = %d", Radio.currFreq);
    devicesTriggerEvent();
}

void ExitBindingMode()
{
    if (!InBindingMode)
    {
        // Not in binding mode
        DBGLN("Cannot exit binding mode, not in binding mode!");
        return;
    }

    // Prevent any new packets from coming in
    Radio.SetTxIdleMode();
    LostConnection();
    // Write the values to eeprom
    config.Commit();

    CRCInitializer = (UID[4] << 8) | UID[5];
    FHSSrandomiseFHSSsequence(uidMacSeedGet());

    #if defined(PLATFORM_ESP32) || defined(PLATFORM_ESP8266)
    webserverPreventAutoStart = true;
    #endif

    // Force RF cycling to start at the beginning immediately
    scanIndex = RATE_MAX;
    RFmodeLastCycled = 0;

    // Do this last as LostConnection() will wait for a tock that never comes
    // if we're in binding mode
    InBindingMode = false;
    DBGLN("Exiting binding mode");
    devicesTriggerEvent();
}

void ICACHE_RAM_ATTR OnELRSBindMSP(uint8_t* packet)
{
    for (int i = 1; i <=4; i++)
    {
        UID[i + 1] = packet[i];
    }

    DBGLN("New UID = %d, %d, %d, %d, %d, %d", UID[0], UID[1], UID[2], UID[3], UID[4], UID[5]);

    // Set new UID in eeprom
    config.SetUID(UID);

    // Set eeprom byte to indicate RX is bound
    config.SetIsBound(true);

    // EEPROM commit will happen on the main thread in ExitBindingMode()
}

void UpdateModelMatch(uint8_t model)
{
    DBGLN("Set ModelId=%u", model);

    config.SetModelId(model);
    config.Commit();
    // This will be called from ProcessRFPacket(), schedule a disconnect
    // in the main loop once the ISR has exited
    connectionState = disconnectPending;
}
