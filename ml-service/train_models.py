"""
Train all 4 ML models and save them to models/.
Run: python train_models.py
"""
import os
import json
import joblib
import numpy as np
import pandas as pd
from sklearn.ensemble import GradientBoostingClassifier, RandomForestClassifier
from sklearn.preprocessing import LabelEncoder, MultiLabelBinarizer, MinMaxScaler
from sklearn.model_selection import train_test_split, StratifiedKFold, cross_val_score
from sklearn.feature_extraction.text import TfidfVectorizer
from sklearn.metrics import (
    classification_report, f1_score, recall_score, precision_score, accuracy_score
)
from sklearn.pipeline import Pipeline
from sklearn.compose import ColumnTransformer
from sklearn.impute import SimpleImputer
from imblearn.over_sampling import SMOTE

os.makedirs("models", exist_ok=True)


# ─── Data Loading ──────────────────────────────────────────────────────────────

def load_data(from_db: bool = False) -> pd.DataFrame:
    if from_db:
        db_url = os.environ.get("DATABASE_URL")
        if not db_url:
            raise RuntimeError("DATABASE_URL not set — cannot load from database")
        # SQLAlchemy expects postgresql:// not postgres://
        db_url = db_url.replace("postgres://", "postgresql://", 1)
        from sqlalchemy import create_engine, text
        engine = create_engine(db_url)
        query = text("""
            SELECT
                patient_id, gender, age, blood_group, genotype,
                height_cm, weight_kg, disease_type, symptoms,
                existing_conditions, severity_level, severity_ordinal,
                weather_condition, smoking_status, alcohol_consumption,
                exercise_habits, diet_type, water_source, patient_category,
                predictive_risk_score, mortality_risk,
                readmission_prediction, drug_recommendation
            FROM patient_training_data
            ORDER BY id
        """)
        with engine.connect() as conn:
            df = pd.read_sql(query, conn)
        print(f"Loaded {len(df)} rows from patient_training_data")
        if len(df) < 50:
            print("WARNING: fewer than 50 training rows — falling back to CSV")
            return _load_csv()
        df["smoking_status"]      = df["smoking_status"].astype(int)
        df["alcohol_consumption"] = df["alcohol_consumption"].astype(int)
        df["symptoms"]            = df["symptoms"].fillna("")
        df["existing_conditions"] = df["existing_conditions"].fillna("None")
        return df
    return _load_csv()


def _load_csv() -> pd.DataFrame:
    path = "data/patients_training.csv"
    if not os.path.exists(path):
        raise FileNotFoundError(
            "Training data not found. Run: python generate_training_data.py"
        )
    # keep_default_na=False: pandas' default NA sentinel list includes
    # literal strings like "None"/"NA"/"NULL" — without this, any category
    # that happens to be spelled that way gets silently turned into NaN and
    # collapsed into "Unknown" at training time (found via exercise_habits,
    # which was losing ~1/3 of its rows this way).
    df = pd.read_csv(path, keep_default_na=False, na_values=[""])
    for col in ["smoking_status", "alcohol_consumption"]:
        df[col] = df[col].map({"True": True, "False": False, True: True, False: False}).fillna(False).astype(int)
    return df


# ─── Encoders ──────────────────────────────────────────────────────────────────

def build_encoders(df: pd.DataFrame) -> dict:
    """Fit all label encoders, return dict."""
    encoders = {}

    for col in ["blood_group", "genotype", "gender", "disease_type",
                "severity_level", "weather_condition", "patient_category",
                "exercise_habits", "diet_type", "water_source",
                "mortality_risk", "readmission_prediction"]:
        values = df[col].fillna("Unknown")
        le = LabelEncoder()
        # Fit with a synthetic "Unknown" row appended so that class always
        # exists, even when every real row happens to be filled in — this is
        # the fallback target safe_encode() uses at inference time for any
        # category not seen during training, instead of silently guessing
        # class 0 (which could be any real category, e.g. "Daily" exercise).
        le.fit(pd.concat([values, pd.Series(["Unknown"])], ignore_index=True))
        df[col + "_enc"] = le.transform(values)
        encoders[col] = le

    # TF-IDF for text fields
    tfidf_symptoms = TfidfVectorizer(max_features=30, ngram_range=(1, 2))
    symptoms_matrix = tfidf_symptoms.fit_transform(df["symptoms"].fillna(""))
    symptoms_cols = [f"sym_{i}" for i in range(symptoms_matrix.shape[1])]
    df = pd.concat([df, pd.DataFrame(symptoms_matrix.toarray(), columns=symptoms_cols)], axis=1)
    encoders["tfidf_symptoms"] = tfidf_symptoms
    encoders["symptoms_cols"] = symptoms_cols

    tfidf_conditions = TfidfVectorizer(max_features=20, ngram_range=(1, 2))
    cond_matrix = tfidf_conditions.fit_transform(df["existing_conditions"].fillna("None"))
    cond_cols = [f"cond_{i}" for i in range(cond_matrix.shape[1])]
    df = pd.concat([df, pd.DataFrame(cond_matrix.toarray(), columns=cond_cols)], axis=1)
    encoders["tfidf_conditions"] = tfidf_conditions
    encoders["conditions_cols"] = cond_cols

    scaler = MinMaxScaler()
    fit_df = df[["age", "height_cm", "weight_kg", "predictive_risk_score"]].fillna(0)
    df[["age_norm", "height_norm", "weight_norm", "risk_norm"]] = scaler.fit_transform(fit_df)
    # Store feature names so inference calls don't trigger sklearn warnings
    encoders["scaler_feature_names"] = ["age", "height_cm", "weight_kg", "predictive_risk_score"]
    encoders["scaler"] = scaler

    return df, encoders


# ─── Model 1: Diagnosis (GradientBoosting) ─────────────────────────────────────

def train_diagnosis_model(df: pd.DataFrame, encoders: dict):
    print("\n=== Model 1: Diagnosis ===")

    feature_cols = (
        encoders["symptoms_cols"]
        + encoders["conditions_cols"]
        + ["blood_group_enc", "genotype_enc", "gender_enc", "age_norm",
           "smoking_status", "alcohol_consumption"]
    )

    X = df[feature_cols].values
    y = df["disease_type_enc"].values

    X_train, X_test, y_train, y_test = train_test_split(
        X, y, test_size=0.2, random_state=42, stratify=y
    )

    model = GradientBoostingClassifier(
        n_estimators=150, max_depth=4, learning_rate=0.08,
        subsample=0.8, random_state=42
    )
    model.fit(X_train, y_train)

    y_pred = model.predict(X_test)
    f1 = f1_score(y_test, y_pred, average="macro")
    print(f"F1 (macro): {f1:.4f}")
    # Report only classes actually present — encoders["disease_type"].classes_
    # also includes the synthetic "Unknown" fallback class (see build_encoders)
    # which never appears as a real label here, so passing all classes_ as
    # target_names mismatches classification_report's inferred label count.
    present_labels = sorted(set(y_test) | set(y_pred))
    present_names = [str(encoders["disease_type"].classes_[i]) for i in present_labels]
    print(classification_report(y_test, y_pred, labels=present_labels, target_names=present_names))

    joblib.dump(model, "models/diagnosis_model.pkl")
    print("Saved models/diagnosis_model.pkl")
    return f1


# ─── Model 2: Risk (RandomForest) ──────────────────────────────────────────────

def train_risk_model(df: pd.DataFrame, encoders: dict):
    print("\n=== Model 2: Risk ===")

    feature_cols = [
        "age_norm", "genotype_enc", "severity_ordinal",
        "smoking_status", "alcohol_consumption",
        "blood_group_enc", "height_norm", "weight_norm",
    ] + encoders["conditions_cols"]

    X = df[feature_cols].values
    y = df["mortality_risk_enc"].values

    X_train, X_test, y_train, y_test = train_test_split(
        X, y, test_size=0.2, random_state=42, stratify=y
    )

    # SMOTE for class imbalance (High-risk patients are minority)
    try:
        sm = SMOTE(random_state=42, k_neighbors=3)
        X_train, y_train = sm.fit_resample(X_train, y_train)
    except ValueError:
        pass  # not enough minority samples with small datasets

    model = RandomForestClassifier(
        n_estimators=200, max_depth=8, min_samples_leaf=2,
        class_weight="balanced", random_state=42, n_jobs=-1
    )
    model.fit(X_train, y_train)

    y_pred = model.predict(X_test)
    present_labels = sorted(set(y_test) | set(y_pred))
    present_names = [str(encoders["mortality_risk"].classes_[i]) for i in present_labels]
    high_label_str = "High"
    high_labels_enc = [
        i for i, c in enumerate(encoders["mortality_risk"].classes_) if c == high_label_str
    ]
    recall_high = recall_score(
        y_test, y_pred,
        labels=high_labels_enc,
        average="macro",
        zero_division=0,
    ) if high_labels_enc else 0.0
    f1 = f1_score(y_test, y_pred, average="macro")
    print(f"F1 (macro): {f1:.4f}  |  Recall (High): {recall_high:.4f}")
    print(classification_report(y_test, y_pred, labels=present_labels, target_names=present_names))

    joblib.dump(model, "models/risk_model.pkl")
    print("Saved models/risk_model.pkl")
    return recall_high


# ─── Model 3: Recommendations (Decision Tree — Phase 1) ────────────────────────

def train_recommendation_model(df: pd.DataFrame, encoders: dict):
    """
    Phase 1: ensemble model on condition+lifestyle → drug recommendation.
    Replace with collaborative filtering once 1000+ real patients collected.

    Was a single DecisionTreeClassifier (macro F1 ~0.3-0.4) — a lone tree
    this shallow overfits to whichever split it happened to make first and
    generalizes poorly on a 9-feature/multi-class problem. Bagging many
    trees (RandomForest) with class_weight="balanced" trades that variance
    for materially better held-out F1 on the same features/data; it does not
    fix the model being trained on synthetic data, so /predict/recommendation
    still flags low_confidence and this is still not a substitute for
    clinician review.
    """
    print("\n=== Model 3: Recommendation (Phase 1 — Random Forest) ===")

    feature_cols = [
        "disease_type_enc", "risk_norm", "smoking_status", "alcohol_consumption",
        "exercise_habits_enc", "diet_type_enc", "water_source_enc",
        "weather_condition_enc", "genotype_enc",
    ]

    drug_le = LabelEncoder()
    df["drug_enc"] = drug_le.fit_transform(df["drug_recommendation"].fillna("Consult specialist"))

    X = df[feature_cols].values
    y = df["drug_enc"].values

    X_train, X_test, y_train, y_test = train_test_split(
        X, y, test_size=0.2, random_state=42, stratify=y
    )

    model = RandomForestClassifier(
        n_estimators=200, max_depth=8, min_samples_leaf=2,
        class_weight="balanced", random_state=42, n_jobs=-1,
    )
    model.fit(X_train, y_train)

    f1 = f1_score(y_test, model.predict(X_test), average="macro")
    print(f"F1 (macro): {f1:.4f}")

    joblib.dump(model, "models/recommendation_model.pkl")
    joblib.dump(drug_le, "models/drug_label_encoder.pkl")
    print("Saved models/recommendation_model.pkl")
    return f1


# ─── Model 4: Routing (Rule-based — no training needed yet) ────────────────────

def build_routing_rules():
    """
    Deterministic routing matrix — no ML until 6 months of operation data.
    Saved as a JSON config file consumed by the FastAPI service.
    """
    print("\n=== Model 4: Routing (Rule-based) ===")
    rules = {
        "disease_to_department": {
            "MentalHealth": "Psychiatry",
            "Genetic": "Genetics",
            "Infectious": "Infectious Disease",
            "Chronic": "General Medicine",
        },
        "category_override": {
            "Child": "Paediatrics",
            "Elderly": "Geriatrics",
        },
        "severity_to_priority": {
            "Critical": 1,
            "Severe": 2,
            "Moderate": 3,
            "Mild": 3,
        },
        "priority_to_route": {
            "1": "emergency",
            "2": "specialist",
            "3": "gp",
        },
    }
    with open("models/routing_rules.json", "w") as f:
        json.dump(rules, f, indent=2)
    print("Saved models/routing_rules.json")


# ─── Persist all encoders ───────────────────────────────────────────────────────

def save_encoders(encoders: dict):
    joblib.dump(encoders, "models/encoders.pkl")
    print("Saved models/encoders.pkl")

    # Also persist class lists as JSON for easy inspection
    classes = {
        k: list(v.classes_) for k, v in encoders.items()
        if isinstance(v, LabelEncoder)
    }
    with open("models/label_classes.json", "w") as f:
        json.dump(classes, f, indent=2)
    print("Saved models/label_classes.json")


# ─── Main ───────────────────────────────────────────────────────────────────────

def main(from_db: bool = False):
    print("Loading training data...")
    df = load_data(from_db=from_db)
    print(f"Loaded {len(df)} rows")

    print("Building encoders...")
    df, encoders = build_encoders(df)

    f1_diag  = train_diagnosis_model(df, encoders)
    rec_risk = train_risk_model(df, encoders)
    f1_rec   = train_recommendation_model(df, encoders)
    build_routing_rules()
    save_encoders(encoders)

    print("\n=== Training Summary ===")
    print(f"  Diagnosis   F1 (macro)  : {f1_diag:.4f}")
    print(f"  Risk        Recall(High): {rec_risk:.4f}")
    print(f"  Recommend   F1 (macro)  : {f1_rec:.4f}")
    print(f"  Routing     Rules-based : ✓")
    print("\nAll models saved to models/")


if __name__ == "__main__":
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("--from-db", action="store_true", help="Load training data from PostgreSQL Gold layer")
    args = parser.parse_args()
    main(from_db=args.from_db)
