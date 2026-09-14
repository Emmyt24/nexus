"""
Generate synthetic patient data for initial model training.
Run once: python generate_training_data.py
Outputs: data/patients_training.csv
"""
import random
import csv
import os
from datetime import date, timedelta

random.seed(42)

DISEASES = ["Infectious", "Chronic", "Genetic", "MentalHealth"]
GENOTYPES = ["AA", "AS", "SS", "AC"]
BLOOD_GROUPS = ["A+", "A-", "B+", "B-", "O+", "O-", "AB+", "AB-"]
GENDERS = ["Male", "Female"]
SEVERITIES = ["Mild", "Moderate", "Severe", "Critical"]
PATIENT_CATEGORIES = ["Child", "Teenager", "Adult", "Elderly"]
WEATHER = ["Dry", "Rainy", "Hot", "Cold", "Humid"]
# "Sedentary", not "None" — pandas' default CSV reader treats the literal
# string "None" as a null sentinel and silently turns it into NaN on read,
# which previously collapsed ~1/3 of rows into the "Unknown" bucket at
# training time (see keep_default_na=False in train_models.py's _load_csv,
# added as the other half of this fix).
EXERCISE = ["Sedentary", "Weekly", "Daily"]
DIET = ["Mixed", "Vegetarian", "Vegan", "Pescatarian"]
WATER = ["Borehole", "Tap", "Bottled", "River"]
STATES = ["Lagos", "Kano", "Abuja", "Rivers", "Oyo", "Kaduna", "Enugu", "Delta"]
OCCUPATIONS = ["Farmer", "Teacher", "Engineer", "Trader", "Nurse", "Driver", "Student", "Unemployed"]

SYMPTOM_MAP = {
    "Infectious": ["Pyrexia, Cephalalgia", "Pyrexia, Emesis, Nausea", "Cough, Dyspnoea", "Pyrexia, Arthralgia, Fatigue", "Cough, Pyrexia, Dyspnoea"],
    "Chronic": ["Cephalalgia, Vertigo", "Fatigue, Dorsalgia", "Chest pain, Dyspnoea", "Fatigue, Oedema", "Nausea, Abdominal pain"],
    "Genetic": ["Arthralgia, Fatigue", "Anaemia, Fatigue", "Dorsalgia, Arthralgia", "Fatigue, Pallor", "Jaundice, Fatigue"],
    "MentalHealth": ["Fatigue, Insomnia", "Anxiety, Palpitations", "Depression, Fatigue", "Psychosis, Agitation", "Mood instability"],
}

CONDITION_MAP = {
    "Infectious": ["None", "Malaria", "Typhoid", "HIV", "Tuberculosis"],
    "Chronic": ["Hypertension", "Diabetes mellitus", "Hypertension, Diabetes mellitus", "Cardiovascular disease", "Asthma"],
    "Genetic": ["Sickle cell disease", "Haemophilia", "Thalassaemia", "Marfan syndrome", "None"],
    "MentalHealth": ["Depression", "Anxiety disorder", "Bipolar disorder", "Schizophrenia", "PTSD"],
}

# Real presentations aren't perfectly disease-specific — without noise, symptoms/
# conditions become a 1:1 lookup table for disease_type and models just memorize
# it (F1=1.0), which is meaningless. These rates make some patients present
# atypically so the text features carry signal without being a perfect proxy.
SYMPTOM_NOISE_RATE = 0.18
CONDITION_NOISE_RATE = 0.12

# Simplified drug map — fewer classes = better F1 with limited data.
# Ordered [mild, moderate, aggressive] per disease — choose_drug() below picks
# an index based on severity/lifestyle/genotype so the label actually
# correlates with the features the recommendation model trains on. Earlier
# this was `random.choice(DRUG_MAP[disease])`, uniform and independent of
# every other column — no model, however good, can beat ~0.3 macro F1
# against a label with zero real signal beyond disease bucket. This mapping
# is a synthetic proxy for "the model has something learnable to find," not
# real clinical guidance — same caveat as the rest of this generator.
DRUG_MAP = {
    "Infectious": ["Amoxicillin 500mg", "Ciprofloxacin 500mg", "Artemether-Lumefantrine"],
    "Chronic":    ["Lisinopril 10mg", "Metformin 500mg", "Amlodipine 5mg"],
    "Genetic":    ["Folic acid 5mg", "Pain management", "Hydroxyurea 500mg"],
    "MentalHealth": ["Fluoxetine 20mg", "Sertraline 50mg", "Olanzapine 5mg"],
}
DRUG_LABEL_NOISE_RATE = 0.15


def choose_drug(disease: str, severity: str, smoking: bool, alcohol: bool, genotype: str) -> str:
    options = DRUG_MAP[disease]
    if severity in ("Severe", "Critical"):
        aggressiveness = 2
    elif smoking or alcohol or genotype == "SS":
        aggressiveness = 1
    else:
        aggressiveness = 0
    if random.random() < DRUG_LABEL_NOISE_RATE:
        aggressiveness = random.randint(0, 2)
    return options[aggressiveness]


def age_to_category(age: int) -> str:
    if age <= 12:   return "Child"
    if age <= 19:   return "Teenager"
    if age <= 64:   return "Adult"
    return "Elderly"


def compute_risk_score(row: dict) -> float:
    score = 0.0
    age = row["age"]
    if age >= 65:   score += 0.25
    elif age >= 50: score += 0.15
    elif age < 5:   score += 0.20

    conds = row["existing_conditions"].lower()
    if "hypertension" in conds: score += 0.10
    if "diabetes"     in conds: score += 0.10
    if "cardiovascular" in conds: score += 0.15

    if row["smoking_status"]:     score += 0.08
    if row["alcohol_consumption"]: score += 0.05
    if row["genotype"] == "SS":   score += 0.15

    if row["severity_level"] == "Critical": score += 0.20
    elif row["severity_level"] == "Severe": score += 0.12

    if row["weather_condition"] == "Rainy" and row["disease_type"] == "Infectious":
        score += 0.05

    return round(min(score, 1.0), 4)


def generate_row(i: int) -> dict:
    disease = random.choice(DISEASES)
    # Force ~20% High-risk samples so the model has enough signal
    force_high_risk = (i % 5 == 0)
    age = random.randint(65, 85) if force_high_risk else random.randint(1, 85)
    genotype = "SS" if force_high_risk else random.choices(GENOTYPES, weights=[50, 30, 10, 10])[0]
    severity = random.choices(["Severe", "Critical"], weights=[60, 40])[0] if force_high_risk else random.choices(SEVERITIES, weights=[35, 35, 20, 10])[0]
    smoking = True if force_high_risk else random.random() < 0.25
    alcohol = True if force_high_risk else random.random() < 0.30
    weather = random.choice(WEATHER)

    symptom_disease = disease
    if random.random() < SYMPTOM_NOISE_RATE:
        symptom_disease = random.choice([d for d in DISEASES if d != disease])

    condition_disease = disease
    if random.random() < CONDITION_NOISE_RATE:
        condition_disease = random.choice([d for d in DISEASES if d != disease])
    conditions = random.choice(CONDITION_MAP[condition_disease])

    row = {
        "patient_id": f"P{1000000 + i}",
        "full_name": f"Patient {i}",
        "gender": random.choice(GENDERS),
        "age": age,
        "blood_group": random.choice(BLOOD_GROUPS),
        "genotype": genotype,
        "height_cm": round(random.uniform(100, 200), 1),
        "weight_kg": round(random.uniform(30, 120), 1),
        "disease_type": disease,
        "symptoms": random.choice(SYMPTOM_MAP[symptom_disease]),
        "existing_conditions": conditions,
        "severity_level": severity,
        "weather_condition": weather,
        "smoking_status": smoking,
        "alcohol_consumption": alcohol,
        "exercise_habits": random.choice(EXERCISE),
        "diet_type": random.choice(DIET),
        "water_source": random.choice(WATER),
        "patient_category": age_to_category(age),
        "state": random.choice(STATES),
        "occupation": random.choice(OCCUPATIONS),
        "drug_recommendation": choose_drug(disease, severity, smoking, alcohol, genotype),
    }

    row["predictive_risk_score"] = compute_risk_score(row)
    score = row["predictive_risk_score"]
    row["mortality_risk"] = "High" if score >= 0.7 else ("Medium" if score >= 0.4 else "Low")
    row["readmission_prediction"] = "High" if (score >= 0.5 or disease == "Chronic") else "Low"

    # Severity label for Model 2 training (ordinal)
    row["severity_ordinal"] = {"Mild": 0, "Moderate": 1, "Severe": 2, "Critical": 3}[severity]
    # Was readmitted (binary outcome label)
    row["was_readmitted"] = 1 if (score > 0.6 and random.random() < 0.65) else 0

    return row


def main():
    os.makedirs("data", exist_ok=True)
    rows = [generate_row(i) for i in range(1500)]

    fieldnames = list(rows[0].keys())
    with open("data/patients_training.csv", "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(rows)

    print(f"Generated {len(rows)} training samples → data/patients_training.csv")
    disease_counts = {}
    for r in rows:
        disease_counts[r["disease_type"]] = disease_counts.get(r["disease_type"], 0) + 1
    print("Disease distribution:", disease_counts)


if __name__ == "__main__":
    main()
