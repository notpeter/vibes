//
// Arrow Illusion — a Playdate rendering of the "moving stripes / changing
// arrows" optical illusion.
//
// The stripes drift at a perfectly constant rate and direction, always.
// Arrow-shaped apertures are laid over them. Because of the aperture
// (barber-pole) effect, your visual system locks the perceived motion to
// the direction the arrows point — so as you crank the crank to spin the
// arrows around, the very same stripes appear to speed up, slow down, and
// reverse. Nothing about the stripe motion ever changes; only the arrows do.
//

#include <stdint.h>
#include <string.h>
#include <math.h>

#include "pd_api.h"

#ifndef M_PI
#define M_PI 3.14159265358979323846
#endif

// -------------------------------------------------------------------------
// Constants
// -------------------------------------------------------------------------

#define SCREEN_W 400
#define SCREEN_H 240

// Height reserved at the top for the caption band.
#define CAPTION_H 22

// Grid of arrows laid across the screen (below the caption band).
#define COLS 4
#define ROWS 3

// Arrow geometry (unrotated, pointing right / +x), in pixels.
#define ARROW_LEN     62.0f  // total length, tip to tail
#define ARROW_SHAFT_T 10.0f  // shaft half-thickness
#define ARROW_HEAD_T  24.0f  // head half-thickness (widest point)
#define ARROW_HEAD_L  28.0f  // length of the triangular head

// The stripe field drifts by advancing this phase every frame. This is the
// "constant rate" — it is deliberately independent of the crank.
#define STRIPE_SPEED 1

static const char* kFontPath = "/System/Fonts/Asheville-Sans-14-Bold.pft";

// -------------------------------------------------------------------------
// State
// -------------------------------------------------------------------------

static PlaydateAPI* pd = NULL;
static LCDFont* font = NULL;

static int   stripePhase = 0;      // advances every frame, constant rate
static float arrowAngleDeg = 0.0f; // direction the arrows point, degrees

// -------------------------------------------------------------------------
// Diagonal stripe pattern (8x8), rebuilt each frame so the stripes drift.
// An LCDPattern is 16 bytes: 8 bytes of pixels followed by 8 bytes of mask.
// -------------------------------------------------------------------------

static uint8_t rotl8(uint8_t v, int n)
{
    n &= 7;
    return (uint8_t)((v << n) | (v >> (8 - n)));
}

static void buildStripePattern(LCDPattern out, int phase)
{
    // Base row: two black pixels, two white — a 4px stripe period.
    const uint8_t base = 0xCC; // 1100 1100

    for (int row = 0; row < 8; row++)
    {
        // Shifting each successive row makes the stripes run on a diagonal;
        // adding the phase makes the whole field drift along that diagonal.
        out[row] = rotl8(base, row + phase);
        out[row + 8] = 0xFF; // fully opaque mask
    }
}

// -------------------------------------------------------------------------
// Build a block-arrow polygon centered at (cx, cy), rotated by angleDeg.
// Fills coords[] with 7 (x, y) integer pairs = 14 ints.
// -------------------------------------------------------------------------

static void buildArrow(int* coords, float cx, float cy, float angleDeg)
{
    const float L  = ARROW_LEN * 0.5f;
    const float t  = ARROW_SHAFT_T;
    const float h  = ARROW_HEAD_T;
    const float hx = L - ARROW_HEAD_L; // x where the head meets the shaft

    // Local points, arrow pointing +x, y downward.
    const float lx[7] = {  L,  hx,  hx, -L, -L,  hx,  hx };
    const float ly[7] = {  0, -h,  -t,  -t,  t,   t,   h };

    const float rad = angleDeg * (float)M_PI / 180.0f;
    const float c = cosf(rad);
    const float s = sinf(rad);

    for (int i = 0; i < 7; i++)
    {
        float x = lx[i] * c - ly[i] * s + cx;
        float y = lx[i] * s + ly[i] * c + cy;
        coords[i * 2]     = (int)lroundf(x);
        coords[i * 2 + 1] = (int)lroundf(y);
    }
}

static void drawArrowOutline(const int* coords)
{
    for (int i = 0; i < 7; i++)
    {
        int j = (i + 1) % 7;
        pd->graphics->drawLine(coords[i * 2], coords[i * 2 + 1],
                               coords[j * 2], coords[j * 2 + 1],
                               2, kColorBlack);
    }
}

// -------------------------------------------------------------------------
// Input: crank drives the arrow direction. When the crank is docked, the
// d-pad turns the arrows instead so the illusion still works on the sim.
// -------------------------------------------------------------------------

static void updateInput(void)
{
    if (pd->system->isCrankDocked())
    {
        PDButtons current;
        pd->system->getButtonState(&current, NULL, NULL);
        if (current & kButtonRight) arrowAngleDeg += 3.0f;
        if (current & kButtonLeft)  arrowAngleDeg -= 3.0f;
    }
    else
    {
        // Crank angle maps directly to the arrow direction.
        arrowAngleDeg = pd->system->getCrankAngle();
    }

    while (arrowAngleDeg < 0.0f)   arrowAngleDeg += 360.0f;
    while (arrowAngleDeg >= 360.0f) arrowAngleDeg -= 360.0f;
}

// -------------------------------------------------------------------------
// Update / draw
// -------------------------------------------------------------------------

static int update(void* userdata)
{
    (void)userdata;

    updateInput();

    // The stripes always advance at the same constant rate.
    stripePhase += STRIPE_SPEED;

    LCDPattern stripes;
    buildStripePattern(stripes, stripePhase);

    pd->graphics->clear(kColorWhite);

    const float cellW = (float)SCREEN_W / COLS;
    const float cellH = (float)(SCREEN_H - CAPTION_H) / ROWS;

    for (int r = 0; r < ROWS; r++)
    {
        for (int col = 0; col < COLS; col++)
        {
            float cx = (col + 0.5f) * cellW;
            float cy = CAPTION_H + (r + 0.5f) * cellH;

            int coords[14];
            buildArrow(coords, cx, cy, arrowAngleDeg);

            // Stripes are only visible inside the arrow apertures.
            pd->graphics->fillPolygon(7, coords, (LCDColor)stripes,
                                      kPolygonFillNonZero);
            drawArrowOutline(coords);
        }
    }

    // Caption band, echoing the original clip.
    pd->graphics->fillRect(0, 0, SCREEN_W, CAPTION_H, kColorWhite);
    pd->graphics->drawLine(0, CAPTION_H, SCREEN_W, CAPTION_H, 1, kColorBlack);
    if (font)
    {
        pd->graphics->setFont(font);
        const char* msg = "Stripes move at a constant rate. Only the arrows change.";
        pd->graphics->drawText(msg, strlen(msg), kASCIIEncoding, 6, 3);
    }

    pd->system->drawFPS(SCREEN_W - 20, SCREEN_H - 16);

    return 1; // tell the system to update the display
}

// -------------------------------------------------------------------------
// Entry point
// -------------------------------------------------------------------------

#ifdef _WINDLL
__declspec(dllexport)
#endif
int eventHandler(PlaydateAPI* playdate, PDSystemEvent event, uint32_t arg)
{
    (void)arg;

    if (event == kEventInit)
    {
        pd = playdate;

        const char* err = NULL;
        font = pd->graphics->loadFont(kFontPath, &err);
        if (font == NULL && err != NULL)
            pd->system->logToConsole("Could not load font %s: %s", kFontPath, err);

        pd->system->setUpdateCallback(update, pd);

        // Smooth crank feel; 30 fps is plenty for the stripe drift.
        pd->display->setRefreshRate(30.0f);
    }

    return 0;
}
